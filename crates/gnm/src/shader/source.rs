//! `ShaderSource` + `ShaderProvider` trait + `ChainProvider` composite (doc-2 §4).
//!
//! The `ShaderProvider` seam is the single route for *all* shader binds so the draw
//! path never special-cases "two shaders" outside a provider (doc-2 §4).
//! `EmbeddedShaderProvider` (phase 3.5) is wired; `GcnShaderProvider` (phase 4) impls
//! the same trait later. [`ChainProvider`] composes the ordered providers behind one
//! `&dyn ShaderProvider` so the executor threads a single provider and a new provider
//! is *added* to the chain, not special-cased into the executor.

use ps4_core::dirty::DirtySource;
use ps4_core::memory::VirtualMemoryManager;
use std::sync::Arc;

/// Logical shader stage. Grows as stages are supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Pixel,
}

/// How a shader entered the PM4 stream (doc-2 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderRef {
    /// `sceGnmSetEmbeddedVs/PsShader(id)` — a fixed built-in shader (phase 3.5).
    Embedded { stage: Stage, id: u32 },
    /// An `OrbShdr` `.sb` GCN binary in guest memory (phase 4). Derived from the
    /// SH-bank `SPI_SHADER_PGM_*` registers at draw time: `addr` is the
    /// GCN code start `((PGM_HI:PGM_LO) << 8)`, and the resource footprint
    /// ([`GcnResources`]) comes from `PGM_RSRC1/2`. Resolving/recompiling this ref
    /// is deferred to phase 4 (P4-18); the draw path currently reports it as
    /// "needs GCN" and skips the draw.
    GcnBinary {
        /// GCN machine-code start address (from `PGM_LO/HI`, `(hi:lo) << 8`).
        addr: u64,
        /// GPR / user-SGPR counts decoded from `PGM_RSRC1/2`.
        res: GcnResources,
        /// Which VS export parameter feeds each PS attribute slot, read from the
        /// CONTEXT-bank `SPI_PS_INPUT_CNTL_n` registers at derive time. Deliberately
        /// NOT part of [`GcnResources`] (that is the GPR/user-SGPR launch footprint);
        /// this is draw-state routing.
        ///
        /// It is part of the shader's IDENTITY, not just its inputs: the same PS
        /// binary under a different routing recompiles to a DIFFERENT SPIR-V module,
        /// so it feeds both the provider's recompile cache key and the pipeline hash
        /// ([`crate::derive`]). Identity (the [`Default`]) for the vertex stage, where
        /// the register is meaningless.
        ps_input_map: ps4_gcn::PsInputMap,
    },
}

/// The GPR / user-SGPR footprint of a GCN shader, decoded from the SH-bank
/// `SPI_SHADER_PGM_RSRC1/2` registers (doc-2 §5, `pm4::opcodes::pgm_rsrc`). Carried
/// on [`ShaderRef::GcnBinary`] so the phase-4 resolver has the launch descriptor
/// without re-reading registers. `Eq` so a `ShaderRef` stays value-comparable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct GcnResources {
    /// Allocated VGPR count (`PGM_RSRC1.VGPRS`).
    pub num_vgprs: u32,
    /// Allocated SGPR count (`PGM_RSRC1.SGPRS`).
    pub num_sgprs: u32,
    /// User-SGPR count (`PGM_RSRC2.USER_SGPR`) — how many SGPRs the driver preloads.
    pub num_user_sgprs: u32,
    /// The guest address of the **fetch shader** the driver preloaded into the VS
    /// user-SGPR pair `s[0:1]` (doc-6 Entry 9). A retail VS opens with a
    /// `s_swappc_b64 s[0:1], s[0:1]` call into this separate subroutine to pull its
    /// vertex attributes; the provider reads the fetch body from here (through the
    /// bounded seam) and inlines it before recompiling. `None`/`0` for a VS that
    /// fetches its own attributes (no `s_swappc`), and for the PS stage (no fetch
    /// call). Sourced in
    /// [`GpuState::gcn_ref_from_regs`](crate::state::GpuState) where the user-SGPR
    /// block is readable; the provider cannot read registers itself. It does NOT feed
    /// the shader-identity hash ([`crate::derive`] hashes only `addr`), so it never
    /// re-keys a pipeline.
    pub fetch_addr: Option<u64>,
}

/// Backend-agnostic resolved shader: SPIR-V today. The executor never sees GCN or
/// SPIR-V bytes directly. Carries a HW-stage role, not a logical stage (doc-2 §C8).
///
/// `io` carries the recompiler's I/O + resource layout ([`ps4_gcn::IoLayout`]) for a
/// GCN-derived shader — the descriptor bindings, `Location` interface, and the
/// load-bearing `num_records` push constant the display-side pipeline wiring (task-52/53)
/// must honor. It is `None` for a firmware-embedded shader, whose fixed host pipeline
/// needs no register-derived layout.
///
/// Cheaply shareable: the GCN provider caches `Arc<HostShader>` and hands the same `Arc`
/// to every re-bind of one shader hash, so a cached resolve is a refcount bump, not a
/// clone of the SPIR-V words.
pub struct HostShader {
    pub stage: Stage,
    pub spirv: Arc<[u32]>,
    pub io: Option<ps4_gcn::IoLayout>,
}

/// Recognized-but-unsupported shader (e.g. phase-4 GCN before the translator lands):
/// a clean defer, never a crash (doc-2 §4).
///
/// It carries the structured [`ps4_gcn::RecompileError`] that fired when the defer was a
/// **recompile** failure on a real `.sb` — moved in, NOT formatted — so the GPU snapshot can
/// name the exact unsupported instruction + dword offset (task-195). The error is formatted
/// (`to_string`) only at the deferral site and only when the snapshot is armed, so the hot
/// path (headless oracle / normal runs) pays no per-defer string allocation. `None` for a
/// coarse defer (parse reject, unmodeled stage, unreadable fetch shader) with no single
/// offending instruction.
#[derive(Debug, Default)]
pub struct ShaderUnsupported {
    /// The recompile error, when this defer was a `RecompileError`; `None` for coarse defers.
    pub recompile_err: Option<ps4_gcn::RecompileError>,
}

impl ShaderUnsupported {
    /// A coarse defer with no per-instruction detail (parse reject, unmodeled stage, …).
    pub fn plain() -> Self {
        Self {
            recompile_err: None,
        }
    }

    /// A recompile-time defer carrying the structured error for later (armed-only) formatting.
    pub fn recompile(err: ps4_gcn::RecompileError) -> Self {
        Self {
            recompile_err: Some(err),
        }
    }
}

/// Turns a `ShaderRef` into a `HostShader`. Providers are chained: `Ok(None)` means
/// "not my kind, try the next"; `Err` means recognized-but-unsupported (doc-2 §4).
///
/// `dirty` is the guest-memory dirty-tracking seam (doc-2 §8.3), threaded through the
/// same way [`crate::cache::ResourceCache::get`] takes one: a provider that caches a
/// recompile keyed on guest bytes (the GCN provider) `watch`es the code range at
/// resolve time so a later guest write to it invalidates the entry on the next
/// per-submit drain — the resource cache's watch-on-insert shape, for shaders. It is
/// `None` for headless / no-VM callers (no invalidation, correct for immutable code),
/// and a clean no-op for a provider whose shaders carry no guest code range (the
/// embedded provider synthesizes from an id).
pub trait ShaderProvider {
    fn resolve(
        &self,
        r: &ShaderRef,
        mem: &dyn VirtualMemoryManager,
        dirty: Option<&dyn DirtySource>,
    ) -> Result<Option<HostShader>, ShaderUnsupported>;
}

/// An ordered composite of [`ShaderProvider`]s — the "SINGLE route for all binds"
/// (doc-2 §4) made concrete. The executor resolves every `ShaderRef` through one
/// `&dyn ShaderProvider`; wrapping the ordered providers here means a new provider
/// (the future GCN one) is *added* to the chain rather than special-cased into the
/// executor.
///
/// Chain semantics per provider, in order:
/// * `Ok(Some(host))` — this provider handled the ref; return it (first win).
/// * `Err(ShaderUnsupported)` — recognized-but-unsupported (e.g. a GCN binary before
///   its translator lands); stop and propagate the clean defer, never fall through.
/// * `Ok(None)` — "not my kind"; try the next provider.
///
/// If every provider yields `Ok(None)`, the composite yields `Ok(None)` — an
/// unhandled ref the caller treats as an unbound bind.
pub struct ChainProvider<'p> {
    providers: &'p [&'p dyn ShaderProvider],
}

impl<'p> ChainProvider<'p> {
    /// Build a composite over the providers in priority order (index 0 tried first).
    pub fn new(providers: &'p [&'p dyn ShaderProvider]) -> Self {
        Self { providers }
    }
}

impl ShaderProvider for ChainProvider<'_> {
    fn resolve(
        &self,
        r: &ShaderRef,
        mem: &dyn VirtualMemoryManager,
        dirty: Option<&dyn DirtySource>,
    ) -> Result<Option<HostShader>, ShaderUnsupported> {
        for p in self.providers {
            match p.resolve(r, mem, dirty)? {
                Some(host) => return Ok(Some(host)),
                None => continue,
            }
        }
        Ok(None)
    }
}
