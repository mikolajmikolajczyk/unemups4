//! Vertex fetch-shader call resolution — `s_swappc_b64` (doc-2 §C4, phase 4;
//! task-113.4.2 AC #7).
//!
//! A retail vertex shader does not fetch its own attributes. The gnmx driver
//! preloads a pointer (in a user-SGPR pair) to a small separate **fetch shader**
//! and the main VS *calls* it near its top with `s_swappc_b64 sdst, ssrc0`
//! (SOP1 op 0x21): the call saves the return PC into `sdst` and jumps to the
//! address in `ssrc0`. The fetch shader loads the vertex-buffer V# descriptors and
//! `buffer_load_format_*`s the per-vertex attributes into an agreed VGPR block,
//! then returns with `s_setpc_b64 s[0:1]` (op 0x20). The main VS then consumes
//! those attribute VGPRs.
//!
//! RE'd from the 5 real Celeste VS (doc-6 Entry 9). Every one opens:
//! ```text
//!   s_mov_b32 vcc_hi, <imm>       ; the universal Orbis prologue
//!   s_swappc_b64 s[0:1], s[0:1]   ; call the fetch shader (its ptr is in s[0:1])
//!   ...                           ; main body reads the fetched v[4:7], exports
//! ```
//! so the fetch-shader pointer arrives in **s[0:1]**, and the call saves the
//! return PC back into that same pair. The fetch body is a *leaf* subroutine
//! (it only ever returns — no nested call, no recursion), so the call/return pair
//! is *exactly* equivalent to splicing the fetch body inline at the call site with
//! its terminating `s_setpc_b64` removed. That equivalence is the whole mechanism:
//! after resolution the combined stream is plain straight-line VS code (SMRD loads,
//! idxen MUBUF fetches, then the main body), which the interpreter oracle
//! ([`crate::run`]) and the recompiler ([`crate::recompile`]) already handle
//! identically — so `s_swappc_b64` needs **no** new interp/recompile op, only this
//! one stream transform, and the differential harness validates it for free.
//!
//! ## Contract (defer, never guess)
//!
//! [`resolve_fetch_call`] inlines the fetch body at the single `s_swappc_b64` the
//! stream contains and returns the resolved [`Decoded`] stream. It is intentionally
//! strict — anything outside the RE'd call/return shape is a clean [`Err`], never a
//! partial or fabricated splice:
//!
//! - the caller must contain exactly one `s_swappc_b64` (zero → nothing to resolve;
//!   two → an unmodeled multi-fetch shape);
//! - its `sdst`/`ssrc0` must be an SGPR pair (the fetch pointer), not a special reg;
//! - the fetch body must be a recognized fetch shader ([`parse_fetch_shader`]
//!   accepts it) and must terminate in exactly one `s_setpc_b64` return, which is
//!   dropped on splice.
//!
//! The resolved stream's `offset_dwords` are renumbered contiguously so a consumer
//! that correlates a `Decoded` back to a stream position stays consistent.

use crate::inst::{Decoded, Inst};
use crate::opcodes;
use crate::operand::Operand;
use crate::{decode_all, parse_fetch_shader};

/// Why a fetch-shader call could not be resolved. Non-panicking, like
/// [`crate::interp::InterpError`] / [`crate::recompile::RecompileError`] — a caller
/// that hits one defers the shader rather than aborting.
#[derive(Clone, PartialEq, Debug, thiserror::Error)]
pub enum FetchResolveError {
    /// The main stream had no `s_swappc_b64` — there is no fetch call to resolve.
    /// (Distinct from an error: a VS that fetches its own attributes needs no
    /// resolution. The caller checks [`has_fetch_call`] first, or treats this as
    /// "return the stream unchanged".)
    #[error("no s_swappc_b64 fetch call in the shader")]
    NoCall,
    /// More than one `s_swappc_b64` — an unmodeled multi-fetch / re-entrant shape.
    #[error("multiple s_swappc_b64 calls are not modeled")]
    MultipleCalls,
    /// The `s_swappc_b64` operands were not the expected SGPR-pair fetch pointer.
    #[error("s_swappc_b64 at dword offset {offset}: {reason}")]
    BadCallShape { offset: u32, reason: &'static str },
    /// The supplied fetch body is not a recognized fetch shader (per
    /// [`parse_fetch_shader`]) — e.g. a non-`buffer_load_format` op, a fetch from an
    /// unloaded V#, or no attribute fetch at all.
    #[error("the fetch-shader body is not a recognized fetch shader")]
    UnrecognizedFetchBody,
    /// The fetch body did not end in exactly one `s_setpc_b64` return.
    #[error("the fetch-shader body has no s_setpc_b64 return to splice out")]
    NoFetchReturn,
}

/// Whether `insts` contains a vertex fetch-shader call (`s_swappc_b64`). The
/// provider uses this to decide whether a VS needs a fetch shader resolved before
/// recompiling: a `true` means the caller must supply the fetch body to
/// [`resolve_fetch_call`]; a `false` means the stream is self-contained.
pub fn has_fetch_call(insts: &[Decoded]) -> bool {
    insts.iter().any(is_swappc)
}

fn is_swappc(d: &Decoded) -> bool {
    matches!(&d.inst, Inst::Sop1 { op, .. } if *op == opcodes::sop1::S_SWAPPC_B64)
}

/// Only `s_setpc_b64` (not the swappc return) — used by tests to assert the fetch's
/// setpc return was spliced out. The resolver locates the return with
/// [`is_fetch_return`], which accepts both.
#[cfg(test)]
fn is_setpc(inst: &Inst) -> bool {
    matches!(inst, Inst::Sop1 { op, .. } if *op == opcodes::sop1::S_SETPC_B64)
}

/// The return that terminates a fetch-shader body. `parse_fetch_shader`
/// (`fetch_shader.rs`) accepts either `s_setpc_b64` (SOP1 op 0x20) or
/// `s_swappc_b64` (op 0x21) as the return, so the splicer must recognize the same
/// set — otherwise a recognized swappc-terminated fetch would find no return to
/// drop and defer as `NoFetchReturn`.
fn is_fetch_return(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Sop1 { op, .. }
            if *op == opcodes::sop1::S_SETPC_B64 || *op == opcodes::sop1::S_SWAPPC_B64
    )
}

/// Resolve a VS's fetch-shader call by inlining `fetch` at the `main` stream's
/// single `s_swappc_b64`, returning the combined straight-line stream (doc-2 §C4;
/// task-113.4.2 AC #7). The result is what [`crate::run`] / [`crate::recompile`]
/// consume — after resolution there is no `s_swappc`/`s_setpc` left, only the
/// fetch's SMRD loads + idxen MUBUF fetches spliced ahead of the rest of the VS.
///
/// `main` is the decoded main VS stream (as from [`decode_all`]); `fetch` is the
/// decoded fetch-shader body the driver pointed the VS at. Both are validated
/// against the RE'd call/return shape (see the module docs); any deviation is a
/// clean [`FetchResolveError`], never a partial splice.
pub fn resolve_fetch_call(
    main: &[Decoded],
    fetch: &[Decoded],
) -> Result<Vec<Decoded>, FetchResolveError> {
    // Exactly one call site. Zero = nothing to resolve; two = unmodeled.
    let mut call_positions = main.iter().enumerate().filter(|(_, d)| is_swappc(d));
    let (call_idx, call) = call_positions.next().ok_or(FetchResolveError::NoCall)?;
    if call_positions.next().is_some() {
        return Err(FetchResolveError::MultipleCalls);
    }

    // The call operands must be an SGPR-pair fetch pointer (sdst = return-PC save,
    // ssrc0 = fetch address). We do not model a fetch pointer that lives in a
    // special register, so reject anything else.
    if let Inst::Sop1 { sdst, ssrc0, .. } = &call.inst {
        let is_sgpr = |o: &Operand| matches!(o, Operand::Sgpr(_));
        if !is_sgpr(ssrc0) {
            return Err(FetchResolveError::BadCallShape {
                offset: call.offset_dwords,
                reason: "s_swappc_b64 fetch address (ssrc0) is not an SGPR pair",
            });
        }
        if !is_sgpr(sdst) {
            return Err(FetchResolveError::BadCallShape {
                offset: call.offset_dwords,
                reason: "s_swappc_b64 return-PC dest (sdst) is not an SGPR pair",
            });
        }
    } else {
        // is_swappc guarantees Sop1; unreachable, but stay total.
        return Err(FetchResolveError::BadCallShape {
            offset: call.offset_dwords,
            reason: "s_swappc_b64 is not a SOP1 instruction",
        });
    }

    // The fetch body must be a recognized fetch shader (real attribute loads), and
    // must terminate in a return — `s_setpc_b64` or `s_swappc_b64`, the same set
    // parse_fetch_shader recognizes — which we splice out.
    if parse_fetch_shader(fetch).is_none() {
        return Err(FetchResolveError::UnrecognizedFetchBody);
    }
    let return_idx = fetch
        .iter()
        .position(|d| is_fetch_return(&d.inst))
        .ok_or(FetchResolveError::NoFetchReturn)?;

    // Build the combined stream: everything before the call, then the fetch body up
    // to (not including) its s_setpc return, then everything after the call. The
    // call and the return both vanish (a leaf call/return with the two dropped is
    // equivalent to the inlined body). `s_waitcnt` filler in the fetch body is kept
    // — the interp/recompiler treat it as a no-op, matching the fetch's semantics.
    let mut out: Vec<Decoded> = Vec::with_capacity(main.len() + return_idx);
    out.extend_from_slice(&main[..call_idx]);
    out.extend_from_slice(&fetch[..return_idx]);
    out.extend_from_slice(&main[call_idx + 1..]);

    // Renumber offset_dwords contiguously by each instruction's own size so a
    // consumer correlating a Decoded back to a stream position stays consistent
    // (the spliced fetch body carried the fetch program's offsets, which would
    // otherwise collide with the main program's).
    let mut pc = 0u32;
    for d in &mut out {
        d.offset_dwords = pc;
        pc += d.size_dwords;
    }
    Ok(out)
}

/// Convenience: decode a raw fetch-shader code window and resolve it into `main`.
/// `fetch_code` is the fetch shader's GCN machine-code dwords (as read from the
/// guest at the address in the VS's fetch-pointer SGPR pair). Equivalent to
/// `resolve_fetch_call(main, &decode_all(fetch_code))`.
pub fn resolve_fetch_call_from_code(
    main: &[Decoded],
    fetch_code: &[u32],
) -> Result<Vec<Decoded>, FetchResolveError> {
    let fetch = decode_all(fetch_code);
    resolve_fetch_call(main, &fetch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_all;

    /// Read a committed corpus `.code.bin` as GCN dwords.
    fn corpus_code(name: &str) -> Vec<u32> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/corpus/{name}.code.bin"));
        let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn detects_the_fetch_call() {
        // The inline-fetch caller opens with the prologue + s_swappc_b64.
        let main = decode_all(&corpus_code("inline_fetch_vs"));
        assert!(has_fetch_call(&main));
        // A self-contained VS (fetches its own attributes) has no call.
        let selfcontained = decode_all(&corpus_code("passthrough_vs"));
        assert!(!has_fetch_call(&selfcontained));
    }

    #[test]
    fn resolves_call_by_inlining_fetch_body() {
        // AC #7: splicing the fetch body at the s_swappc site yields a straight-line
        // stream with NO s_swappc and NO s_setpc left, and the fetch's attribute
        // loads (SMRD + idxen MUBUF) present ahead of the main body's exports.
        let main = decode_all(&corpus_code("inline_fetch_vs"));
        let fetch = decode_all(&corpus_code("fetch_pos_vs"));
        let resolved = resolve_fetch_call(&main, &fetch).expect("resolve");

        assert!(
            !resolved.iter().any(super::is_swappc),
            "the call must be spliced out"
        );
        assert!(
            !resolved.iter().any(|d| super::is_setpc(&d.inst)),
            "the fetch return must be spliced out"
        );
        // The fetch body contributed at least one buffer_load_format (the attribute
        // fetch) ahead of the main body's export.
        let has_fetch_load = resolved
            .iter()
            .any(|d| matches!(&d.inst, Inst::Mubuf { .. }));
        assert!(has_fetch_load, "the fetch's attribute load must be present");
        // offset_dwords is contiguous and consistent with each inst's size.
        let mut pc = 0u32;
        for d in &resolved {
            assert_eq!(d.offset_dwords, pc, "renumbered offsets are contiguous");
            pc += d.size_dwords;
        }
    }

    #[test]
    fn no_call_defers_cleanly() {
        let selfcontained = decode_all(&corpus_code("passthrough_vs"));
        let fetch = decode_all(&corpus_code("fetch_pos_vs"));
        assert_eq!(
            resolve_fetch_call(&selfcontained, &fetch),
            Err(FetchResolveError::NoCall)
        );
    }

    #[test]
    fn unrecognized_fetch_body_defers() {
        // Feeding a non-fetch body (the main VS itself, which s_endpgms and exports)
        // as the "fetch shader" is rejected — never a partial splice.
        let main = decode_all(&corpus_code("inline_fetch_vs"));
        let not_a_fetch = decode_all(&corpus_code("passthrough_vs"));
        assert_eq!(
            resolve_fetch_call(&main, &not_a_fetch),
            Err(FetchResolveError::UnrecognizedFetchBody)
        );
    }

    #[test]
    fn resolves_call_when_fetch_returns_via_swappc() {
        // parse_fetch_shader accepts s_swappc_b64 (SOP1 0x21) as a fetch return, not
        // only s_setpc_b64 (0x20) — the splicer must agree. Re-encode the corpus
        // fetch's terminating s_setpc as s_swappc and confirm it still inlines (rather
        // than degrading to NoFetchReturn).
        let mut fetch = decode_all(&corpus_code("fetch_pos_vs"));
        let mut rewrote = false;
        for d in &mut fetch {
            if let Inst::Sop1 { op, .. } = &mut d.inst
                && *op == opcodes::sop1::S_SETPC_B64
            {
                *op = opcodes::sop1::S_SWAPPC_B64;
                rewrote = true;
            }
        }
        assert!(
            rewrote,
            "corpus fetch body must terminate in an s_setpc to rewrite"
        );

        let main = decode_all(&corpus_code("inline_fetch_vs"));
        let resolved = resolve_fetch_call(&main, &fetch).expect("swappc-terminated fetch resolves");
        assert!(
            !resolved.iter().any(super::is_swappc),
            "both the call and the fetch's swappc return must be spliced out"
        );
        assert!(
            !resolved.iter().any(|d| super::is_setpc(&d.inst)),
            "no s_setpc left"
        );
    }

    #[test]
    fn from_code_matches_decode_then_resolve() {
        let main = decode_all(&corpus_code("inline_fetch_vs"));
        let fetch_code = corpus_code("fetch_pos_vs");
        let a = resolve_fetch_call_from_code(&main, &fetch_code).expect("from_code");
        let b = resolve_fetch_call(&main, &decode_all(&fetch_code)).expect("decode+resolve");
        assert_eq!(a, b);
    }
}
