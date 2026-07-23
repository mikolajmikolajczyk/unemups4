//! GnmDriver HLE state (doc-4 §1): submit-queue bookkeeping, flip labels. Called
//! by the `libSceGnmDriver` HLE handlers in `crates/libs` (thin NID glue → here).
//!
//! Phase-2: the submit entry points record their command-buffer
//! (pointer, size) ranges into `submissions` so the PM4 trace decoder
//! can consume them. Nothing is executed here — record + log only. Vulkan-free.

use std::sync::{Mutex, OnceLock};

use crate::cache::ResourceCache;
use crate::shader::embedded::EmbeddedShaderProvider;
use crate::shader::gcn::GcnShaderProvider;
use crate::shader::pipeline_cache::PipelineCache;
use crate::state::GpuState;

/// One guest-memory command-buffer range handed to a submit entry point. The
/// pointers are identity-mapped guest addresses (guest ptr == host ptr, doc-2 §1),
/// so the PM4 decoder reads them directly via the memory manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitRange {
    /// Draw command buffer (DCB) guest pointer.
    pub dcb_ptr: u64,
    /// DCB size in bytes.
    pub dcb_size: u32,
    /// Constant command buffer (CCB) guest pointer; 0 when absent.
    pub ccb_ptr: u64,
    /// CCB size in bytes; 0 when absent.
    pub ccb_size: u32,
    /// Set when this submission also requested a scanout flip
    /// (`sceGnmSubmitAndFlipCommandBuffers`).
    pub flip: bool,
}

/// GnmDriver HLE state. Records the command buffers the guest submits so the PM4
/// executor can walk them, and owns the submit-spanning [`GpuState`] shadow register
/// file (doc-4 §5/§C7): PS4 context/SH registers persist across submits, so the
/// state must outlive any single per-submit `Executor` — hence its home is here, in
/// the driver singleton, not the executor.
#[derive(Default)]
pub struct GnmDriver {
    submissions: Vec<SubmitRange>,
    submit_batches: u64,
    state: GpuState,
    /// Guest-side host-pipeline cache (doc-4 §4, decision-7). Lives here, alongside the
    /// shadow register file, so a pipeline bound in one submit resolves to the same
    /// [`PipelineId`](ps4_core::gpu::PipelineId) in the next — a re-bind must not emit a
    /// second `CreatePipeline`. The per-submit [`Executor`](crate::exec::Executor)
    /// borrows it `&mut`.
    pipelines: PipelineCache,
    /// Guest-side resource cache (doc-4 §8): upload-on-use / invalidate-on-dirty for the
    /// vertex/index buffers a draw references. Driver-owned so a buffer uploaded in one
    /// submit is not re-uploaded in the next unless the guest dirtied its range.
    resources: ResourceCache,
    /// The firmware-embedded shader provider (doc-4 §4). Owned here so the provider
    /// chain the executor threads persists across submits.
    embedded: EmbeddedShaderProvider,
    /// The GCN `.sb` → recompiled-SPIR-V provider (doc-4 §4, phase 4). Driver-owned so
    /// its hash-keyed recompile cache survives across submits — a re-bind of the same
    /// shader in a later submit is a cache hit, not a re-recompile. Per-submit
    /// [`drain_dirty`](GcnShaderProvider::drain_dirty) invalidates dirtied code ranges.
    gcn: GcnShaderProvider,
}

impl GnmDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a `sceGnmSubmitCommandBuffers` DCB/CCB pair.
    pub fn submit(&mut self, dcb_ptr: u64, dcb_size: u32, ccb_ptr: u64, ccb_size: u32) {
        self.submissions.push(SubmitRange {
            dcb_ptr,
            dcb_size,
            ccb_ptr,
            ccb_size,
            flip: false,
        });
    }

    /// Record a `sceGnmSubmitAndFlipCommandBuffers` DCB/CCB pair plus a flip request.
    pub fn submit_and_flip(&mut self, dcb_ptr: u64, dcb_size: u32, ccb_ptr: u64, ccb_size: u32) {
        self.submissions.push(SubmitRange {
            dcb_ptr,
            dcb_size,
            ccb_ptr,
            ccb_size,
            flip: true,
        });
    }

    /// End-of-batch sync point (`sceGnmSubmitDone`). No execution yet.
    pub fn submit_done(&mut self) {
        self.submit_batches += 1;
    }

    /// Record a draw packet builder call. No PM4 is emitted yet.
    pub fn draw_index(&mut self, index_count: u32, index_addr: u64) {
        let _ = (index_count, index_addr);
    }

    /// Record an auto-index draw packet builder call. No PM4 is emitted yet.
    pub fn draw_index_auto(&mut self, index_count: u32) {
        let _ = index_count;
    }

    /// Record a direct compute dispatch packet builder call. No PM4 is emitted yet.
    pub fn dispatch_direct(
        &mut self,
        thread_groups_x: u32,
        thread_groups_y: u32,
        thread_groups_z: u32,
    ) {
        let _ = (thread_groups_x, thread_groups_y, thread_groups_z);
    }

    /// Record an async-compute queue map. No queue is created yet.
    pub fn map_compute_queue(&mut self, pipe_id: u32, queue_id: u32) {
        let _ = (pipe_id, queue_id);
    }

    /// Record an async-compute doorbell ring. No queue is driven yet.
    pub fn ding_dong(&mut self, queue_id: u32, offset: u32) {
        let _ = (queue_id, offset);
    }

    /// The submissions recorded so far — the seam the PM4 decoder consumes.
    pub fn submissions(&self) -> &[SubmitRange] {
        &self.submissions
    }

    /// Number of `sceGnmSubmitDone` batches seen.
    pub fn submit_batches(&self) -> u64 {
        self.submit_batches
    }

    /// Drain the recorded submissions (for the decoder to take ownership).
    pub fn take_submissions(&mut self) -> Vec<SubmitRange> {
        std::mem::take(&mut self.submissions)
    }

    /// The submit-spanning GPU shadow state (doc-4 §5/§C7). The per-submit
    /// [`Executor`](crate::exec::Executor) borrows this `&mut` while it walks a
    /// submission's PM4 (applying `SET_*_REG` writes / `IT_CLEAR_STATE`); the HLE
    /// shader-bind stubs write into it here too. It lives in the driver so register
    /// state set in one submit is visible to a draw in the next.
    pub fn state_mut(&mut self) -> &mut GpuState {
        &mut self.state
    }

    /// Read-only view of the GPU shadow state (introspection / tests).
    pub fn state(&self) -> &GpuState {
        &self.state
    }

    /// The submit-spanning shadow register file **and** pipeline cache, borrowed
    /// together for the per-submit [`Executor`](crate::exec::Executor). Returned as one
    /// pair because both live in the driver and the executor needs both `&mut` at once;
    /// a single accessor avoids two overlapping `&mut self` borrows at the call site.
    pub fn exec_state_mut(&mut self) -> (&mut GpuState, &mut PipelineCache) {
        (&mut self.state, &mut self.pipelines)
    }

    /// The full set of per-submit executor borrows: the shadow register file, the
    /// pipeline and resource caches (all `&mut`), and the two shader providers (`&`,
    /// interior-mutable). Returned in one call so the caller can build the provider
    /// chain and construct the [`Executor`](crate::exec::Executor) without juggling
    /// several overlapping `&mut self` borrows. The providers are shared refs because
    /// [`ChainProvider`](crate::shader::source::ChainProvider) borrows them `&dyn`; their
    /// caches are behind interior mutability, so a resolve mutates them through `&`.
    #[allow(clippy::type_complexity)]
    pub fn exec_parts(
        &mut self,
    ) -> (
        &mut GpuState,
        &mut PipelineCache,
        &mut ResourceCache,
        &EmbeddedShaderProvider,
        &GcnShaderProvider,
    ) {
        (
            &mut self.state,
            &mut self.pipelines,
            &mut self.resources,
            &self.embedded,
            &self.gcn,
        )
    }

    /// Evict every resource-cache entry whose backing range overlaps the guest range the
    /// guest just freed/unmapped (doc-4 §8), appending the resulting
    /// [`BackendCmd::FreeResource`](ps4_core::gpu::BackendCmd::FreeResource) teardown
    /// commands to `out`. The [`MemoryFreeSink`](ps4_core::gpu::MemoryFreeSink) impl calls
    /// this while holding the driver lock, then ships `out` across the display channel so
    /// the backend frees/revokes the vk resources. The GCN shader-recompile cache is left
    /// alone: it keys on shader-code hashes and re-recompiles a dirtied/reloaded range on
    /// its own `drain_dirty`, independent of buffer lifetime.
    pub fn free_resource_range(
        &mut self,
        addr: u64,
        size: u64,
        dirty: &dyn ps4_core::dirty::DirtySource,
        out: &mut Vec<ps4_core::gpu::BackendCmd>,
    ) {
        self.resources.free_range(addr, size, dirty, out);
    }
}

static DRIVER: OnceLock<Mutex<GnmDriver>> = OnceLock::new();

/// The process-global GnmDriver, mirroring the `ps4-core` `get_kernel` OnceLock
/// pattern (doc-4 §0: state in a well-known place, reached at runtime). The thin
/// `libSceGnmDriver` NID handlers reach the driver through this, and it owns the
/// submit-spanning [`GpuState`] shadow register file.
///
/// ## Lock invariant (load-bearing — the display thread must NEVER lock this)
///
/// `record_submit` (`libs/…/submit.rs`) holds this mutex across the whole
/// `exec.run(...)`, and `exec.run` blocks on the display channel (`run_command_list`
/// / `submit_and_flip` → `rx.recv()`). With [`GpuState`] now living in the driver,
/// that lock-hold is intentional: the executor mutates driver-owned state while a
/// present is in flight. Consequently **the display thread must never acquire
/// `driver()`** — if it did, the guest thread would be blocked on the display
/// channel while holding the driver lock, and the display thread would block on the
/// driver lock: an instant deadlock. Every other guest-thread Gnm HLE call also
/// serializes behind an in-flight present; that is acceptable for the phase-4
/// corpus, but a future multi-threaded change must not reintroduce a display-thread
/// driver lock — the deadlock invariant is load-bearing, do not break it.
pub fn driver() -> &'static Mutex<GnmDriver> {
    DRIVER.get_or_init(|| Mutex::new(GnmDriver::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_records_range() {
        let mut d = GnmDriver::new();
        d.submit(0x1000, 256, 0x2000, 64);
        assert_eq!(
            d.submissions(),
            &[SubmitRange {
                dcb_ptr: 0x1000,
                dcb_size: 256,
                ccb_ptr: 0x2000,
                ccb_size: 64,
                flip: false,
            }]
        );
    }

    #[test]
    fn submit_and_flip_marks_flip() {
        let mut d = GnmDriver::new();
        d.submit_and_flip(0x4000, 512, 0, 0);
        let r = d.submissions()[0];
        assert!(r.flip);
        assert_eq!(r.dcb_ptr, 0x4000);
        assert_eq!(r.dcb_size, 512);
    }

    #[test]
    fn submit_done_counts_batches() {
        let mut d = GnmDriver::new();
        d.submit_done();
        d.submit_done();
        assert_eq!(d.submit_batches(), 2);
    }

    #[test]
    fn take_submissions_drains() {
        let mut d = GnmDriver::new();
        d.submit(1, 2, 3, 4);
        let taken = d.take_submissions();
        assert_eq!(taken.len(), 1);
        assert!(d.submissions().is_empty());
    }
}
