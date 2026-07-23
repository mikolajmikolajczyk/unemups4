//! `EmbeddedShaderProvider` (doc-4 §4, phase 3.5): embedded-id → hardcoded host
//! SPIR-V for the fixed fullscreen-quad VS + R/G-export PS pair (doc-3 §3.4). No
//! GCN — the emulator synthesizes host shaders from the ID and never sees a `.sb`
//! blob.
//!
//! This is the SINGLE route for every shader bind (doc-4 §4): the executor resolves
//! *all* `ShaderRef`s through a `ShaderProvider`, so nothing special-cases "there
//! are only two shaders" outside this provider. A `GcnBinary` ref returns
//! `Err(ShaderUnsupported)` — the clean phase-4 defer, never a crash.

use ps4_core::dirty::DirtySource;
use ps4_core::memory::VirtualMemoryManager;
use std::sync::Arc;

use crate::shader::source::{HostShader, ShaderProvider, ShaderRef, ShaderUnsupported, Stage};

/// The firmware-embedded vertex-shader id for the fullscreen quad (doc-3 §3.4:
/// `sceGnmSetEmbeddedVsShader`, `shaderid 0`).
pub const EMBEDDED_VS_FULLSCREEN_QUAD: u32 = 0;
/// The firmware-embedded pixel-shader id that exports 32-bit R and G (doc-3 §3.4:
/// `sceGnmSetEmbeddedPsShader`, `shaderid 1`). Id 0 is the *empty* PS and is not
/// synthesized here — only the R/G-export variant produces a visible fill.
pub const EMBEDDED_PS_RG_EXPORT: u32 = 1;

/// The hand-authored host SPIR-V for the two embedded shaders (doc-3 §3.4). Built
/// from `crates/gnm/shaders/*.{vert,frag}` via `glslc` (core GLSL 4.5 → SPIR-V,
/// `spirv-val` clean), kept inside the Vulkan portability subset — no capability
/// beyond a single RGBA color export and `gl_VertexIndex`, MoltenVK/Metal safe
/// (decision-3). Baked as bytes and reinterpreted to `&[u32]` at resolve time; the
/// `.spv` blobs are 4-byte aligned by construction (checked in tests).
const VS_FULLSCREEN_QUAD_SPV: &[u8] = include_bytes!("../../shaders/embedded_fullscreen.vert.spv");
const PS_RG_EXPORT_SPV: &[u8] = include_bytes!("../../shaders/embedded_rg_export.frag.spv");

/// The host SPIR-V bytes for an embedded shader `(stage, id)` pair, or `None` if this
/// phase does not synthesize it. Lets the ash backend (`ps4-gpu`) build the pipeline
/// from the same blobs the [`EmbeddedShaderProvider`] resolves, without `ps4-gnm`
/// naming any `ash::vk` type — the backend receives raw SPIR-V, keeping the
/// Vulkan-free boundary (doc-4 §1). The returned slice is 4-byte aligned.
pub fn embedded_spirv(stage: Stage, id: u32) -> Option<&'static [u8]> {
    match (stage, id) {
        (Stage::Vertex, EMBEDDED_VS_FULLSCREEN_QUAD) => Some(VS_FULLSCREEN_QUAD_SPV),
        (Stage::Pixel, EMBEDDED_PS_RG_EXPORT) => Some(PS_RG_EXPORT_SPV),
        _ => None,
    }
}

/// Reinterpret a committed `.spv` byte blob as SPIR-V words. The blobs are produced
/// by `glslc` and are always a whole number of 32-bit words in host (little-endian)
/// order; `chunks_exact(4)` drops nothing for a valid module.
fn spirv_words(bytes: &[u8]) -> Arc<[u32]> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Resolves firmware-embedded shader ids to hardcoded host SPIR-V (phase 3.5). The
/// only provider wired today; a `GcnShaderProvider` (phase 4) is chained after it.
#[derive(Default)]
pub struct EmbeddedShaderProvider;

impl EmbeddedShaderProvider {
    pub fn new() -> Self {
        Self
    }
}

impl ShaderProvider for EmbeddedShaderProvider {
    /// `dirty` is ignored: an embedded shader is synthesized from its id and carries no
    /// guest code range to watch, so there is nothing to invalidate on a guest write —
    /// a clean no-op (the dirty seam is the GCN provider's, which caches recompiles of
    /// guest bytes).
    fn resolve(
        &self,
        r: &ShaderRef,
        _mem: &dyn VirtualMemoryManager,
        _dirty: Option<&dyn DirtySource>,
    ) -> Result<Option<HostShader>, ShaderUnsupported> {
        match *r {
            ShaderRef::Embedded {
                stage: Stage::Vertex,
                id: EMBEDDED_VS_FULLSCREEN_QUAD,
            } => Ok(Some(HostShader {
                stage: Stage::Vertex,
                spirv: spirv_words(VS_FULLSCREEN_QUAD_SPV),
                io: None,
            })),
            ShaderRef::Embedded {
                stage: Stage::Pixel,
                id: EMBEDDED_PS_RG_EXPORT,
            } => Ok(Some(HostShader {
                stage: Stage::Pixel,
                spirv: spirv_words(PS_RG_EXPORT_SPV),
                io: None,
            })),
            // A recognized embedded stage/id this phase does not synthesize (e.g.
            // the empty PS id 0, or an unmapped id): "not my kind" → chain onward.
            // No later provider handles embedded ids, so the executor treats a
            // None on an embedded bind as an unbound draw and skips it.
            ShaderRef::Embedded { .. } => Ok(None),
            // A real `.sb` GCN binary is not this provider's kind: "not my kind" →
            // chain onward to the `GcnShaderProvider` that parses + recompiles it
            // (doc-4 §4). This MUST be `Ok(None)`, not `Err` — the `ChainProvider`
            // propagates `Err` and would short-circuit before the GCN provider runs.
            ShaderRef::GcnBinary { .. } => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};

    /// A do-nothing memory manager: the embedded provider never reads guest memory
    /// (its shaders are synthesized from the id), so every method is a stub. Only
    /// the trait-object shape is needed to call `resolve`.
    struct NullMem;
    impl VirtualMemoryManager for NullMem {
        fn map(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
            _name: Option<&str>,
        ) -> Result<u64, &'static str> {
            Err("stub")
        }
        fn unmap(&mut self, _addr: u64, _size: usize) -> Result<(), &'static str> {
            Err("stub")
        }
        fn protect(
            &mut self,
            _addr: u64,
            _size: usize,
            _prot: MemoryProtection,
        ) -> Result<(), &'static str> {
            Err("stub")
        }
        unsafe fn get_host_ptr(&self, _addr: u64) -> Option<*mut u8> {
            None
        }
        fn find_free_region(&mut self, _size: usize) -> u64 {
            0
        }
        fn is_memory_free(&self, _addr: u64, _size: usize) -> bool {
            false
        }
    }

    fn provider() -> EmbeddedShaderProvider {
        EmbeddedShaderProvider::new()
    }

    #[test]
    fn resolves_embedded_vs_id0_to_spirv() {
        let mem = NullMem;
        let r = ShaderRef::Embedded {
            stage: Stage::Vertex,
            id: EMBEDDED_VS_FULLSCREEN_QUAD,
        };
        let host = provider()
            .resolve(&r, &mem, None)
            .unwrap()
            .expect("VS resolved");
        assert_eq!(host.stage, Stage::Vertex);
        // A valid SPIR-V module starts with the magic 0x0723_0203.
        assert_eq!(host.spirv[0], 0x0723_0203);
        assert!(host.spirv.len() > 4);
    }

    #[test]
    fn resolves_embedded_ps_id1_to_spirv() {
        let mem = NullMem;
        let r = ShaderRef::Embedded {
            stage: Stage::Pixel,
            id: EMBEDDED_PS_RG_EXPORT,
        };
        let host = provider()
            .resolve(&r, &mem, None)
            .unwrap()
            .expect("PS resolved");
        assert_eq!(host.stage, Stage::Pixel);
        assert_eq!(host.spirv[0], 0x0723_0203);
    }

    #[test]
    fn gcn_binary_chains_onward_not_err() {
        // A real .sb GCN shader is "not my kind" → Ok(None) so the ChainProvider reaches
        // the GcnShaderProvider. An Err here would short-circuit the chain (the phase-4
        // regression this guards): the GCN provider would never run.
        let mem = NullMem;
        let r = ShaderRef::GcnBinary {
            addr: 0xE000,
            res: crate::shader::source::GcnResources::default(),
        };
        assert!(matches!(provider().resolve(&r, &mem, None), Ok(None)));
    }

    #[test]
    fn unmapped_embedded_id_chains_onward() {
        // An embedded id this phase doesn't synthesize (the empty PS id 0) is
        // Ok(None), not an error: "not my kind", chain to the next provider.
        let mem = NullMem;
        let r = ShaderRef::Embedded {
            stage: Stage::Pixel,
            id: 0,
        };
        assert!(matches!(provider().resolve(&r, &mem, None), Ok(None)));
    }

    #[test]
    fn baked_spirv_is_word_aligned() {
        // include_bytes! blobs must be a whole number of 32-bit words or the
        // chunks_exact reinterpret would silently drop the tail.
        assert_eq!(VS_FULLSCREEN_QUAD_SPV.len() % 4, 0);
        assert_eq!(PS_RG_EXPORT_SPV.len() % 4, 0);
    }
}
