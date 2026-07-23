//! Fetch-shader parsing (doc-4 §C4, phase 4): recover a VS's vertex-attribute
//! layout from the small GCN subroutine the driver points it at.
//!
//! A **fetch shader** is not the vertex shader. It is a short GCN subroutine the
//! gnmx driver preloads a pointer to (in user SGPRs) and the main VS *calls* to
//! pull its vertex attributes. The convention (shadPS4 / GPCS4):
//!
//! 1. one or more `s_load_dwordx4` load a buffer resource (a 128-bit V#) from the
//!    descriptor set an SGPR pair points at, into a destination SGPR quad;
//! 2. one `buffer_load_format_*` per attribute reads that V# — `idxen`, so the
//!    per-lane vertex index in `v0` selects the element — into an agreed
//!    destination VGPR block;
//! 3. `s_setpc_b64` (or `s_swappc_b64`) returns to the caller.
//!
//! This parser walks the decoded stream ([`decode_all`](crate::decode_all)) and
//! recovers, per attribute, `{ V#-source SGPR, destination VGPR, component count,
//! and how the V# is reached (which descriptor-set pointer SGPR + byte offset) }`.
//! It does **not execute** the subroutine: the recovered table replaces it — it
//! feeds the recompiled SSBO loads / the Vulkan vertex-input layout. The parse is
//! plain data (no Vulkan, no memory reads); the caller reads the actual V# bytes
//! separately through the bounded seam.
//!
//! ## Defer, never guess (AC #3)
//!
//! A stream that is not the expected `s_load_dwordx4* buffer_load_format_* …
//! s_setpc` shape is **not** a fetch shader this parser understands — it returns
//! [`None`] (a clean defer) rather than a partial or fabricated table. A
//! `buffer_load_format` whose V#-source SGPR was never loaded by a preceding
//! `s_load_dwordx4`, a non-idxen fetch, or an unmodeled instruction between the
//! loads and the fetches all defer. Nothing here panics.

use std::collections::HashMap;

use crate::inst::Inst;
use crate::operand::Operand;
use crate::{Decoded, opcodes};

/// One vertex attribute recovered from a fetch shader (doc-4 §C4). Plain data —
/// no Vulkan type — so `ps4-gcn` stays backend-free; the caller folds this into
/// the vertex-input pipeline state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchAttribute {
    /// The first SGPR of the V# (128-bit buffer resource, 4 SGPRs) this attribute
    /// is fetched from — the `srsrc` of its `buffer_load_format_*`.
    pub vsharp_sgpr: u8,
    /// The first destination VGPR the fetch writes (one per component).
    pub dest_vgpr: u8,
    /// Number of components fetched (1..=4), from the `buffer_load_format_x/xy/
    /// xyz/xyzw` opcode.
    pub components: u8,
    /// The user-SGPR pair holding the pointer to the descriptor set this V# was
    /// loaded from (the `sbase` of the `s_load_dwordx4` that filled `vsharp_sgpr`).
    /// Lets the caller reach the real V# bytes without re-walking the loads.
    pub desc_ptr_sgpr: u8,
    /// Byte offset of this V# within that descriptor set (the `s_load_dwordx4`
    /// immediate offset, in dwords, converted to bytes).
    pub desc_offset_bytes: u64,
}

/// The recovered vertex-attribute layout of a fetch shader (doc-4 §C4): the
/// attributes in fetch (declaration) order. Value-comparable so it can seed a
/// pipeline key or be asserted against a hand-reasoned expected table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchLayout {
    /// One entry per `buffer_load_format_*`, in stream order.
    pub attributes: Vec<FetchAttribute>,
}

/// SMRD `s_load_dwordx4` immediate offsets are in **dwords** (4 bytes each) on
/// SI/CI. The parser converts to a byte offset so the recovered
/// [`FetchAttribute::desc_offset_bytes`] matches a byte-addressed descriptor set.
const SMRD_OFFSET_DWORD_BYTES: u64 = 4;

/// Parse a decoded GCN fetch-shader subroutine into its vertex-attribute layout
/// (doc-4 §C4). Returns [`None`] when the stream is not the recognized fetch-shader
/// shape — a clean defer, never a panic or a bogus/partial table (AC #3).
///
/// Recognized shape (in order, ignoring `s_waitcnt`/`s_nop` filler):
///   1. zero or more `s_load_dwordx4 sdst4, sbaseN, imm` — each records "SGPR
///      `sdst` now holds a V# loaded from descriptor-set pointer `sbaseN` at byte
///      offset `imm*4`";
///   2. one or more `buffer_load_format_{x,xy,xyz,xyzw} vdataN, v0, srsrcN, …
///      idxen` — each yields one [`FetchAttribute`]; its `srsrc` MUST name a V#
///      an earlier load produced (else defer), and it MUST be `idxen` (a
///      vertex-index-driven fetch — the fetch-shader convention);
///   3. exactly one `s_setpc_b64` / `s_swappc_b64` return, which ends the parse.
///
/// Anything else — an unmodeled instruction, a non-idxen fetch, a fetch from an
/// unloaded SGPR, a store, no return, or no attributes — makes this not a
/// fetch shader we recover, so it returns [`None`].
pub fn parse_fetch_shader(insts: &[Decoded]) -> Option<FetchLayout> {
    // SGPR (V# quad base) → (descriptor-set pointer SGPR, byte offset) recorded by
    // each s_load_dwordx4 so a later buffer_load_format's srsrc resolves to it.
    let mut loaded: HashMap<u8, (u8, u64)> = HashMap::new();
    let mut attributes = Vec::new();
    let mut saw_return = false;

    for d in insts {
        match &d.inst {
            // Filler between the loads and the fetches: harmless, skip. `s_waitcnt`
            // (SOPP) and `s_nop` carry no attribute information.
            Inst::Sopp { op, .. }
                if *op == opcodes::sopp::S_WAITCNT || *op == opcodes::sopp::S_NOP => {}

            // A V# load into an SGPR quad — record its source so a later fetch's
            // srsrc resolves. Only the 128-bit `s_load_dwordx4` form seeds a V#.
            Inst::Smrd {
                op,
                sdst,
                sbase,
                imm,
                offset,
            } if *op == opcodes::smrd::S_LOAD_DWORDX4 => {
                // The destination must be a concrete SGPR, and the offset an
                // immediate (a register-indexed load is outside this convention →
                // defer).
                let Operand::Sgpr(dst) = *sdst else {
                    return None;
                };
                if !*imm {
                    return None;
                }
                let byte_offset = u64::from(*offset) * SMRD_OFFSET_DWORD_BYTES;
                loaded.insert(dst, (*sbase, byte_offset));
            }

            // One attribute fetch. It must be a `buffer_load_format_*`, idxen, from
            // a V# a preceding load produced.
            Inst::Mubuf {
                op,
                vdata,
                srsrc,
                idxen,
                ..
            } => {
                let Some(components) = opcodes::mubuf::vdata_count(*op) else {
                    // Not a modeled buffer op (e.g. a store, or an unmapped op).
                    return None;
                };
                // Only the *_FORMAT_* loads are attribute fetches. `vdata_count`
                // also covers `buffer_load_dword` / stores; restrict to the format
                // loads the convention uses.
                if !is_format_load(*op) {
                    return None;
                }
                // idxen is the fetch-shader convention: the vertex index in v0
                // selects the element. A non-idxen fetch is not this shape.
                if !*idxen {
                    return None;
                }
                let Operand::Vgpr(dest_vgpr) = *vdata else {
                    return None;
                };
                // The V# source SGPR must have been loaded by an earlier
                // s_load_dwordx4 — otherwise we cannot say where the attribute
                // comes from, so defer rather than fabricate.
                let &(desc_ptr_sgpr, desc_offset_bytes) = loaded.get(srsrc)?;
                attributes.push(FetchAttribute {
                    vsharp_sgpr: *srsrc,
                    dest_vgpr,
                    components,
                    desc_ptr_sgpr,
                    desc_offset_bytes,
                });
            }

            // The return that ends a fetch shader.
            Inst::Sop1 { op, .. }
                if *op == opcodes::sop1::S_SETPC_B64 || *op == opcodes::sop1::S_SWAPPC_B64 =>
            {
                saw_return = true;
                break;
            }

            // Anything else is outside the recognized fetch-shader shape.
            _ => return None,
        }
    }

    // A fetch shader must return, and must fetch at least one attribute. A stream
    // with neither is not one we recovered → defer.
    if !saw_return || attributes.is_empty() {
        return None;
    }
    Some(FetchLayout { attributes })
}

/// Whether a MUBUF opcode is one of the `buffer_load_format_{x,xy,xyz,xyzw}`
/// attribute loads (as opposed to `buffer_load_dword` or a store).
fn is_format_load(op: u8) -> bool {
    use opcodes::mubuf::*;
    matches!(
        op,
        BUFFER_LOAD_FORMAT_X
            | BUFFER_LOAD_FORMAT_XY
            | BUFFER_LOAD_FORMAT_XYZ
            | BUFFER_LOAD_FORMAT_XYZW
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_all;

    /// The committed hand-assembled `fetch_vs` corpus (real GFX7 GCN bytes from
    /// llvm-mc; see `tests/corpus/fetch_vs.s`). Loaded here so the parser is
    /// exercised against real machine code, not a synthetic in-test encoding.
    fn fetch_vs_code() -> Vec<u32> {
        let p =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/fetch_vs.code.bin");
        let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        assert!(bytes.len().is_multiple_of(4), "code is dword-aligned");
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn parses_corpus_fetch_shader_to_expected_table() {
        // AC #1: the hand-assembled fetch shader parses to the attribute table
        // hand-REASONED from its semantics (NOT captured from the parser):
        //   s_load_dwordx4 s[8:11],  s[2:3], 0x0  -> V# in s8  from s2 @ 0
        //   s_load_dwordx4 s[12:15], s[2:3], 0x4  -> V# in s12 from s2 @ 0x4*4=16
        //   buffer_load_format_xyzw v[4:7], v0, s[8:11]  idxen -> v4, 4 comps, s8
        //   buffer_load_format_xy   v[8:9], v0, s[12:15] idxen -> v8, 2 comps, s12
        //   s_setpc_b64 s[0:1]                    -> return
        let insts = decode_all(&fetch_vs_code());
        let layout = parse_fetch_shader(&insts).expect("recognized fetch shader");

        let want = vec![
            FetchAttribute {
                vsharp_sgpr: 8,
                dest_vgpr: 4,
                components: 4,
                desc_ptr_sgpr: 2,
                desc_offset_bytes: 0,
            },
            FetchAttribute {
                vsharp_sgpr: 12,
                dest_vgpr: 8,
                components: 2,
                desc_ptr_sgpr: 2,
                desc_offset_bytes: 16,
            },
        ];
        assert_eq!(layout.attributes, want);
    }

    #[test]
    fn passthrough_vs_is_not_a_fetch_shader() {
        // AC #3: the main VS (passthrough_vs) fetches an attribute but ends in
        // s_endpgm and exports — it is NOT a bare fetch subroutine, so it defers.
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/corpus/passthrough_vs.code.bin");
        let bytes = std::fs::read(&p).unwrap();
        let code: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let insts = decode_all(&code);
        // It has exports (EXP) and s_endpgm, which are outside the fetch shape.
        assert_eq!(parse_fetch_shader(&insts), None);
    }

    #[test]
    fn fetch_from_unloaded_sgpr_defers() {
        // AC #3: a buffer_load_format from a V# SGPR no s_load_dwordx4 produced has
        // no known source → defer, not a fabricated attribute. Just the fetch + a
        // return, with no preceding load.
        // buffer_load_format_xyzw v[4:7], v0, s[8:11], 0 idxen ; then s_setpc_b64.
        let bytes: [u8; 12] = [
            0x00, 0x20, 0x0c, 0xe0, 0x00, 0x04, 0x02, 0x80, // buffer_load_format_xyzw idxen
            0x00, 0x20, 0x80, 0xbe, // s_setpc_b64 s[0:1]
        ];
        let code: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let insts = decode_all(&code);
        assert_eq!(parse_fetch_shader(&insts), None);
    }

    #[test]
    fn no_return_defers() {
        // AC #3: a load-then-fetch with no s_setpc return is not a complete fetch
        // subroutine → defer.
        let bytes: [u8; 12] = [
            0x00, 0x03, 0x84, 0xc0, // s_load_dwordx4 s[8:11], s[2:3], 0x0
            0x00, 0x20, 0x0c, 0xe0, // buffer_load_format_xyzw (first dword)
            0x00, 0x04, 0x02, 0x80, // (second dword) v[4:7], v0, s[8:11] idxen
        ];
        let code: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let insts = decode_all(&code);
        assert_eq!(parse_fetch_shader(&insts), None);
    }

    #[test]
    fn empty_or_return_only_defers() {
        // A bare return with no attribute fetch is not a fetch shader.
        let bytes: [u8; 4] = [0x00, 0x20, 0x80, 0xbe]; // s_setpc_b64 s[0:1]
        let code: Vec<u32> = vec![u32::from_le_bytes(bytes)];
        let insts = decode_all(&code);
        assert_eq!(parse_fetch_shader(&insts), None);
        // And an entirely empty stream.
        assert_eq!(parse_fetch_shader(&[]), None);
    }
}
