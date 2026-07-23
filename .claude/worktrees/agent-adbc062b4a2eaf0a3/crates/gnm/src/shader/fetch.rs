//! Fetch-shader → vertex-input state (doc-4 §C4, phase 4).
//!
//! The GCN [decoder](ps4_gcn::decode_all) + [fetch-shader parser](ps4_gcn::parse_fetch_shader)
//! recover, from the fetch subroutine's machine code, which V# (buffer resource)
//! each attribute is fetched from and into which destination VGPR. The `.sb`
//! `VertexInputSemantic` table separately says which *semantic index* each
//! destination VGPR carries. This module **merges** the two — by destination VGPR
//! — into the vertex-input layout the pipeline key needs:
//! `{ semantic → V#-slot SGPR, destination VGPR, component count }`.
//!
//! It **does not execute** the fetch shader: it reads the fetch-shader machine code
//! through the bounded seam (the fetch-shader address is register-derived and
//! untrusted, exactly like the `.sb` shader address), parses it as *data*, and the
//! recovered table replaces the subroutine. Nothing here touches Vulkan — the
//! result is plain data the display-side pipeline (later task) folds into its
//! vertex-input state.

use ps4_core::bounded_read::BoundedRead;
use ps4_gcn::{FetchAttribute, decode_all, parse_fetch_shader};

use crate::shader::sb::{SbParseError, Semantics};

/// One resolved vertex attribute: the fetch-shader source (V#-slot SGPR + component
/// count, from the parsed fetch subroutine) merged with the `.sb` semantic index the
/// attribute's destination VGPR carries (doc-4 §C4). Plain data — the vertex-input
/// slice of the pipeline key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedAttribute {
    /// The `.sb` `VertexInputSemantic` index this attribute fills (the shader-visible
    /// attribute location the VS reads).
    pub semantic: u8,
    /// The first SGPR of the V# (buffer resource) this attribute is fetched from,
    /// recovered from the fetch shader's `buffer_load_format` `srsrc`.
    pub vsharp_sgpr: u8,
    /// The destination VGPR the fetch writes — the join key between the fetch shader
    /// (which knows the VGPR) and the semantic table (which names the VGPR).
    pub dest_vgpr: u8,
    /// Component count fetched (1..=4), from the `buffer_load_format_x/xy/xyz/xyzw`
    /// opcode.
    pub components: u8,
    /// The user-SGPR pair holding the pointer to the descriptor set the V# was loaded
    /// from (so the caller can reach the actual V# bytes).
    pub desc_ptr_sgpr: u8,
    /// Byte offset of the V# within that descriptor set.
    pub desc_offset_bytes: u64,
}

/// The vertex-input state recovered from a fetch shader (doc-4 §C4): the resolved
/// attributes in fetch order, value-comparable so it seeds a pipeline key. This is
/// the "attribute → V# layout" AC #2 produces WITHOUT executing the fetch blob.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VertexInputState {
    /// One entry per fetched attribute, in fetch (declaration) order.
    pub attributes: Vec<ResolvedAttribute>,
}

/// Why recovering a fetch shader's vertex-input state yielded nothing usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchResolveError {
    /// Reading the fetch-shader machine code through the bounded seam faulted
    /// (unmapped / straddling a mapping) — never an over-read.
    MemoryFault,
    /// The fetch-shader code length was not a whole number of dwords (corrupt).
    Truncated,
    /// The machine code is not the recognized fetch-shader shape (see
    /// [`ps4_gcn::parse_fetch_shader`]) — a clean defer, not a fabricated table.
    NotAFetchShader,
    /// A `buffer_load_format` wrote a destination VGPR no `VertexInputSemantic`
    /// entry names, so its semantic index is unknown — defer rather than guess.
    UnmappedVgpr(u8),
}

impl std::fmt::Display for FetchResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchResolveError::MemoryFault => write!(f, "fetch-shader code read faulted"),
            FetchResolveError::Truncated => write!(f, "fetch-shader code truncated"),
            FetchResolveError::NotAFetchShader => {
                write!(f, "code is not the recognized fetch-shader shape")
            }
            FetchResolveError::UnmappedVgpr(v) => {
                write!(f, "no VertexInputSemantic names destination VGPR v{v}")
            }
        }
    }
}

impl std::error::Error for FetchResolveError {}

/// Recover a fetch shader's vertex-input state (doc-4 §C4). Reads the fetch-shader
/// machine code at `fetch_addr` (`len` bytes) through the **bounded** seam — the
/// address is register-derived and untrusted, so this is range-validated, never a
/// bare identity over-read — decodes it, parses the fetch convention, and merges the
/// recovered per-attribute table with the `.sb` [`Semantics`] `VertexInputSemantic`
/// table by destination VGPR.
///
/// Does NOT execute the fetch shader: the returned [`VertexInputState`] replaces it.
/// A non-conforming fetch shader, a faulting read, or an attribute whose destination
/// VGPR no semantic names all defer cleanly (AC #3) — never a panic or a partial
/// table.
pub fn resolve_fetch_vertex_input(
    fetch_addr: u64,
    len: usize,
    reader: &(impl BoundedRead + ?Sized),
    semantics: &Semantics,
) -> Result<VertexInputState, FetchResolveError> {
    let bytes = reader
        .read_ranged(fetch_addr, len)
        .map_err(|_| FetchResolveError::MemoryFault)?;
    if !bytes.len().is_multiple_of(4) {
        return Err(FetchResolveError::Truncated);
    }
    let code: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let insts = decode_all(&code);
    let layout = parse_fetch_shader(&insts).ok_or(FetchResolveError::NotAFetchShader)?;
    merge_with_semantics(&layout.attributes, semantics)
}

/// Merge the parsed fetch-shader attributes with the `.sb` `VertexInputSemantic`
/// table by destination VGPR (doc-4 §C4). Each fetch attribute writes a destination
/// VGPR; the semantic table names the semantic index that VGPR carries. An attribute
/// whose VGPR no semantic names is a clean defer (`UnmappedVgpr`), not a guess.
fn merge_with_semantics(
    attrs: &[FetchAttribute],
    semantics: &Semantics,
) -> Result<VertexInputState, FetchResolveError> {
    let mut attributes = Vec::with_capacity(attrs.len());
    for a in attrs {
        // Find the semantic entry whose destination VGPR matches this fetch's.
        let sem = semantics
            .vertex_inputs
            .iter()
            .find(|s| s.vgpr == a.dest_vgpr)
            .ok_or(FetchResolveError::UnmappedVgpr(a.dest_vgpr))?;
        attributes.push(ResolvedAttribute {
            semantic: sem.semantic,
            vsharp_sgpr: a.vsharp_sgpr,
            dest_vgpr: a.dest_vgpr,
            components: a.components,
            desc_ptr_sgpr: a.desc_ptr_sgpr,
            desc_offset_bytes: a.desc_offset_bytes,
        });
    }
    Ok(VertexInputState { attributes })
}

/// Map a [`FetchResolveError`] onto the `.sb` parser's error type so the provider can
/// funnel a fetch-resolve defer through the same clean-defer path a `.sb` parse reject
/// uses (a `MemoryFault`/`Truncated` are the shared shapes; the fetch-specific reasons
/// map to `MemoryFault` as the nearest recognized-but-unusable signal).
impl From<FetchResolveError> for SbParseError {
    fn from(e: FetchResolveError) -> Self {
        match e {
            FetchResolveError::Truncated => SbParseError::Truncated,
            _ => SbParseError::MemoryFault,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shader::sb::VertexInputSemantic;

    /// The committed hand-assembled `fetch_vs` corpus GCN bytes (real GFX7 encodings;
    /// see `crates/gcn/tests/corpus/fetch_vs.s`).
    fn fetch_vs_bytes() -> Vec<u8> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gcn/tests/corpus/fetch_vs.code.bin");
        std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    /// A flat backing-buffer bounded reader: guest addr == `base + index`, bounds-
    /// checked so an over-read is a clean fault. The minimal [`BoundedRead`] seam.
    struct BufMem {
        base: u64,
        buf: Vec<u8>,
    }

    impl BoundedRead for BufMem {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            let start = addr.checked_sub(self.base).ok_or("segfault")? as usize;
            let end = start.checked_add(size).ok_or("segfault")?;
            if end > self.buf.len() {
                return Err("segfault");
            }
            Ok(self.buf[start..end].to_vec())
        }
    }

    /// The `.sb` VertexInputSemantic table for the corpus fetch shader, hand-reasoned
    /// from its ABI: destination v4 carries semantic 0 (position), destination v8
    /// carries semantic 1 (color). Independent of the parser under test.
    fn corpus_semantics() -> Semantics {
        Semantics {
            vertex_inputs: vec![
                VertexInputSemantic {
                    semantic: 0,
                    vgpr: 4,
                    size_in_elements: 4,
                },
                VertexInputSemantic {
                    semantic: 1,
                    vgpr: 8,
                    size_in_elements: 2,
                },
            ],
            ..Semantics::default()
        }
    }

    #[test]
    fn resolves_corpus_fetch_to_vertex_input_state() {
        // AC #2: a VS resolve consumes the parsed fetch table + the .sb semantic table
        // and produces the vertex-input state (attribute → V# layout) WITHOUT executing
        // the fetch blob. Expected values are hand-reasoned:
        //   attr v4 ← V# s8, 4 comps, semantic 0
        //   attr v8 ← V# s12, 2 comps, semantic 1
        const BASE: u64 = 0x0030_0000;
        let mem = BufMem {
            base: BASE,
            buf: fetch_vs_bytes(),
        };
        let len = mem.buf.len();
        let state = resolve_fetch_vertex_input(BASE, len, &mem, &corpus_semantics())
            .expect("recognized fetch shader");

        let want = vec![
            ResolvedAttribute {
                semantic: 0,
                vsharp_sgpr: 8,
                dest_vgpr: 4,
                components: 4,
                desc_ptr_sgpr: 2,
                desc_offset_bytes: 0,
            },
            ResolvedAttribute {
                semantic: 1,
                vsharp_sgpr: 12,
                dest_vgpr: 8,
                components: 2,
                desc_ptr_sgpr: 2,
                desc_offset_bytes: 16,
            },
        ];
        assert_eq!(state.attributes, want);
    }

    #[test]
    fn non_conforming_code_defers() {
        // AC #3: garbage that is not the fetch-shader shape defers cleanly (no panic).
        const BASE: u64 = 0x0040_0000;
        let mem = BufMem {
            base: BASE,
            buf: vec![0xFFu8; 16], // all-ones dwords → Inst::Unknown → not a fetch shape
        };
        assert_eq!(
            resolve_fetch_vertex_input(BASE, 16, &mem, &corpus_semantics()),
            Err(FetchResolveError::NotAFetchShader)
        );
    }

    #[test]
    fn unmapped_read_defers() {
        // AC #3: a fetch-shader address the bounded seam cannot satisfy faults cleanly.
        let mem = BufMem {
            base: 0x0010_0000,
            buf: vec![0u8; 64],
        };
        assert_eq!(
            resolve_fetch_vertex_input(0xDEAD_0000, 16, &mem, &corpus_semantics()),
            Err(FetchResolveError::MemoryFault)
        );
    }

    #[test]
    fn attribute_vgpr_with_no_semantic_defers() {
        // AC #3: the fetch shader writes v4 and v8, but the semantic table names only
        // v4 — the v8 attribute has no semantic, so the merge defers rather than guess.
        const BASE: u64 = 0x0050_0000;
        let mem = BufMem {
            base: BASE,
            buf: fetch_vs_bytes(),
        };
        let len = mem.buf.len();
        let partial = Semantics {
            vertex_inputs: vec![VertexInputSemantic {
                semantic: 0,
                vgpr: 4,
                size_in_elements: 4,
            }],
            ..Semantics::default()
        };
        assert_eq!(
            resolve_fetch_vertex_input(BASE, len, &mem, &partial),
            Err(FetchResolveError::UnmappedVgpr(8))
        );
    }
}
