//! The executor (doc-2 §1, §3): consumes a decoded PM4 stream and acts on it. It
//! is backend-agnostic and Vulkan-free — present crosses the display-thread channel
//! through the `ps4_core::gpu::PresentSink` seam, and GPU→CPU sync (EOP/EOS labels)
//! is a synchronous write into identity-mapped guest memory. Phases 3.5/4 add draw
//! and shader arms here; the loop is written once (doc-2 §3 "one pipeline").
//!
//! Phase 3: `ExecMode::PresentSubset`. Decode+trace still run in every
//! mode; this file adds the *present/sync* arms on top:
//!
//!  * `SubmitAndFlip` (the submit range's flip flag) → drive the existing softgpu
//!    present path via [`PresentSink::submit_and_flip`] (reused).
//!  * `IT_EVENT_WRITE_EOP` / `IT_EVENT_WRITE_EOS` → write the label value to the
//!    guest address the packet names, so a guest that submits then waits on the
//!    EOP label proceeds (doc-2 §C2 timeline model; synchronous for now, no async
//!    GPU thread).
//!
//! No draws, no shaders, no state application yet. Unknown/unhandled
//! opcodes are decoded and skipped, never fatal.

use std::sync::atomic::Ordering;

use ps4_core::bounded_read::bounded_read;
use ps4_core::dirty::{AlwaysDirty, DirtySource, dirty_source};
use ps4_core::gpu::{
    BackendCmd, IndexType, MAX_VERTEX_ATTRIBUTES, PresentSink, PushConstantRange, ReadbackPolicy,
    ResourceId, SamplerAddressMode, SamplerDesc, SamplerFilter, ScissorRect, StorageBinding,
    TargetDesc, TextureBinding, TextureFormat, VertexAttr, VertexBinding, VertexFormat,
    VertexLayout, ViewportRect,
};
use ps4_core::memory::MemoryAccessExt;

use crate::cache::{Compression, Extent, SurfaceFormat, SurfaceLayout, TexelSize, Tiling};
use crate::cache::{ResLayout, ResourceCache, ResourceKey};
use crate::derive::{Scissor, Viewport};
use crate::driver::SubmitRange;
use crate::idmem::IdentityMem;
use crate::pm4::decode::{self, Pm4Packet};
use crate::pm4::opcodes::op;
use crate::shader::pipeline_cache::{PipelineCache, PipelineLookup};
use crate::shader::sb::{SbParseError, SbShader, parse_sb};
use crate::shader::source::{HostShader, ShaderProvider, Stage};
use crate::state::{BoundShaders, GpuState};
use crate::vbuf::{
    BufferSlot, DrawBuffers, FetchLayout, TextureBindingRange, TextureDesc, UserData,
    derive_buffer_ranges, derive_texture,
};

/// Which packet families the executor acts on. Decode+trace run in every mode; the
/// mode gates *execution* so trace-only, present-subset and
/// full draw are three configurations of one loop, not forks (doc-2 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    /// Decode + trace only, no backend touched.
    TraceOnly,
    /// Present + GPU→CPU sync arms on; no draws/shaders (phase 3).
    PresentSubset,
    /// Present/sync arms **plus** the embedded-shader draw arm (phase 3.5).
    /// A superset of [`PresentSubset`]: everything it does, and additionally an
    /// `IT_DRAW_INDEX_AUTO` bound to embedded VS/PS ids dispatches a hardcoded host
    /// pipeline (via the [`PresentSink::run_command_list`] seam). A draw bound to a
    /// real `.sb` GCN shader is cleanly deferred ("needs GCN (phase 4)"), never
    /// fatal (AC #3).
    Draw,
}

impl ExecMode {
    /// Whether the present/sync arms (SubmitAndFlip, EOP/EOS) run in this mode.
    /// Both `PresentSubset` and its superset `Draw` enable them.
    fn present_sync_on(self) -> bool {
        matches!(self, ExecMode::PresentSubset | ExecMode::Draw)
    }
}

/// Executes a present/sync/draw subset of PM4 against a [`PresentSink`]. Holds no
/// Vulkan state; the sink ships present across the display-thread channel and the
/// EOP/EOS arms write guest memory directly (identity-mapped, doc-2 §1).
///
/// It borrows the submit-spanning [`GpuState`] (`&mut`) from the driver rather than
/// owning it: `SET_*_REG` writes and shader binds from earlier submits must persist
/// into later ones (doc-2 §5/§C7), and the executor is created fresh per submit. See
/// the [`driver()`](crate::driver::driver) lock invariant — the executor mutates
/// this state while a present is in flight.
pub struct Executor<'a> {
    mode: ExecMode,
    sink: &'a dyn PresentSink,
    state: &'a mut GpuState,
    /// The single route every draw's VS/PS bind resolves through (doc-2 §4). A
    /// composite ([`ChainProvider`](crate::shader::source::ChainProvider)) built by
    /// the caller: embedded today, with the GCN provider appended later. The executor
    /// only knows the `ShaderProvider` seam, so a new provider is added to the chain
    /// rather than special-cased here.
    providers: &'a dyn ShaderProvider,
    /// Guest-side host-pipeline cache (doc-2 §4, decision-7). Borrowed from the driver
    /// so a pipeline bound in an earlier submit resolves to the same id here — the miss
    /// path (which emits `CreatePipeline`) runs at most once per distinct pipeline.
    pipelines: &'a mut PipelineCache,
    /// Guest-side resource cache (doc-2 §8): the draw pulls its referenced vertex/index
    /// buffers through this (upload-on-use, dirty-invalidate). Driver-owned so a buffer
    /// uploaded in an earlier submit is reused here.
    resources: &'a mut ResourceCache,
    /// The dirty-tracking source the resource cache watches ranges against (doc-2 §8.3).
    /// The registered x86jit-backed source when wired, else an [`AlwaysDirty`] fallback
    /// (re-upload every submit — correct, not incremental). Held for the executor's life
    /// so `ResourceCache::get`'s `watch` calls reach one stable source.
    dirty: std::sync::Arc<dyn DirtySource>,
    /// Whether a draw into an offscreen render target is read back into its guest range
    /// (doc-2 §8.5, task-56 step 5). Resolved once from `UNEMUPS4_RT_READBACK` at
    /// construction: the default [`ReadbackPolicy::Off`] emits ZERO readback commands (the
    /// portable path — RT contents stay GPU-side and are only sampled host-side), while
    /// [`ReadbackPolicy::All`] appends a [`BackendCmd::ReadbackRenderTarget`] per flagged RT
    /// so the backend copies the RT out, re-tiles, and writes the guest range.
    readback_policy: ReadbackPolicy,
    /// Readbacks queued by [`Self::register_render_target`] under
    /// [`ReadbackPolicy::All`], flushed into the submit's command list AFTER the whole PM4
    /// walk (so every producer draw has been recorded before its RT is read back). One entry
    /// per distinct flagged RT range; empty under [`ReadbackPolicy::Off`]. Each entry is
    /// `(id, addr, size, pitch, tiling)` — the guest surface's row stride and tile mode ride
    /// along because only the executor's [`TargetDesc`] knows them (task-181).
    pending_readbacks: Vec<(ResourceId, u64, u64, u32, ps4_core::gpu::Tiling)>,
    /// Render-target PNG dumps queued by [`Self::register_render_target`] for an ARMED
    /// snapshot capture (task-187), flushed into the submit's command list after the PM4
    /// walk for the same reason the readbacks are: the copy must see a rendered RT.
    ///
    /// A deliberately SEPARATE queue from [`Self::pending_readbacks`], not a flag on it.
    /// The readback reproduces the guest's tiled layout in guest memory and refuses layouts
    /// it cannot express; this writes a PNG from the host image and touches guest memory
    /// never. Fusing them is what left the diagnostic unable to show a macro-tiled Celeste
    /// target (task-181), which is the whole reason task-187 exists.
    pending_rt_dumps: Vec<(ResourceId, std::path::PathBuf)>,
}

impl<'a> Executor<'a> {
    pub fn new(
        mode: ExecMode,
        sink: &'a dyn PresentSink,
        state: &'a mut GpuState,
        providers: &'a dyn ShaderProvider,
        pipelines: &'a mut PipelineCache,
        resources: &'a mut ResourceCache,
    ) -> Self {
        // The registered dirty source when wired (the real x86jit watched-range facility),
        // else the conservative AlwaysDirty fallback (headless / no VM): re-upload every
        // submit, which is correct if not incremental (doc-2 §8.3).
        let dirty = dirty_source().unwrap_or_else(|| std::sync::Arc::new(AlwaysDirty::new()));
        Self {
            mode,
            sink,
            state,
            providers,
            pipelines,
            resources,
            dirty,
            // Off by default (readback is a perf cliff); opt-in via UNEMUPS4_RT_READBACK.
            readback_policy: ReadbackPolicy::from_env(),
            pending_readbacks: Vec::new(),
            pending_rt_dumps: Vec::new(),
        }
    }

    /// Execute one guest submission: walk its DCB/CCB PM4 (reusing the
    /// decoder), apply the present/sync arms, and — when the submission carries a
    /// flip and the mode allows it — present through the sink.
    ///
    /// # Safety
    /// Same contract as [`decode::decode_submit_range`]: the range's DCB/CCB
    /// pointers must reference readable, identity-mapped guest command-buffer memory
    /// for the duration of the call. EOP/EOS arms additionally write to the
    /// (identity-mapped) label addresses the packets name.
    pub unsafe fn run(&mut self, range: &SubmitRange) {
        if !self.mode.present_sync_on() {
            return;
        }
        let _span = tracing::debug_span!("pm4_exec").entered();
        let prof = crate::profile::enabled();
        let t_run = prof.then(std::time::Instant::now);
        let draw_on = self.mode == ExecMode::Draw;
        // The submitted DCB/CCB are borrowed where they lie and walked as a packet
        // stream: no dword copy of the (multi-megabyte, retail) command buffer and no
        // per-packet body allocation. `decode_ns` now covers only obtaining the views;
        // the walk itself is what is left of `run_ns`.
        let t_decode = prof.then(std::time::Instant::now);
        let _decode_span = tracing::debug_span!("pm4_decode").entered();
        let dcb = unsafe { decode::guest_words(range.dcb_ptr, range.dcb_size) };
        let ccb = unsafe { decode::guest_words(range.ccb_ptr, range.ccb_size) };
        if let Some(t) = t_decode {
            crate::profile::EXEC
                .decode_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            crate::profile::EXEC.runs.fetch_add(1, Ordering::Relaxed);
        }
        drop(_decode_span);
        // ONE command list per submit (doc-2 §3 data-list model): every draw in this
        // submission appends its pipeline/bind/draw commands here, and the whole list
        // ships once after the walk. The display thread records it into ONE command
        // buffer behind the per-frame fence — no per-draw channel round-trip / GPU stall.
        let mut cmds: Vec<BackendCmd> = Vec::new();
        // Index-draw state accumulated across the submit walk (doc-2 §5): the last
        // `IT_INDEX_TYPE` width and the last `IT_INDEX_BASE` address seed a following
        // `IT_DRAW_INDEX_2` (which also carries its own base — that supersedes). Reset
        // per submit; a draw before any index-type packet defaults to 16-bit indices.
        let mut index_state = IndexState::default();
        // Pull-driven instrumentation (doc-6): opcodes decoded but not yet acted on, logged
        // once per distinct opcode per submit so a retail command buffer's real packet mix
        // surfaces without spamming the log with every repeat.
        let mut seen_unhandled: Vec<u8> = Vec::new();
        let mut packet_count: u64 = 0;
        for pkt in decode::decode(&dcb).chain(decode::decode(&ccb)) {
            packet_count += 1;
            if let Pm4Packet::Type3 { opcode, body, .. } = pkt {
                match opcode {
                    op::IT_EVENT_WRITE_EOP => unsafe { write_eop_label(body) },
                    op::IT_EVENT_WRITE_EOS => unsafe { write_eos_label(body) },
                    // §C7 shadow register file: SET_*_REG packets just write their
                    // body into the matching bank of the submit-spanning GpuState.
                    // Applied in every present_sync mode (PresentSubset up); no
                    // register index is interpreted yet (the draw path derives
                    // pipeline state from these at draw time).
                    op::IT_SET_CONTEXT_REG
                    | op::IT_SET_SH_REG
                    | op::IT_SET_UCONFIG_REG
                    | op::IT_SET_CONFIG_REG => {
                        if let Some(base) = crate::pm4::opcodes::set_reg_base(opcode) {
                            self.state.apply_set_reg(base, body);
                        }
                    }
                    // IT_CLEAR_STATE resets the register banks (CONTEXT_CONTROL /
                    // CLEAR_STATE full-clear, doc-2 §C7). The bound-shader view is
                    // left intact (a separate guest bind, re-emitted per draw).
                    op::IT_CLEAR_STATE => self.state.clear_regs(),
                    // Index-draw state (doc-2 §5): these packets only record state; the
                    // draw arms below consume it. Applied in Draw mode (they gate the
                    // indexed-draw path).
                    op::IT_INDEX_TYPE if draw_on => index_state.set_type(body),
                    op::IT_INDEX_BASE if draw_on => index_state.set_base(body),
                    // NUM_INSTANCES carries the instance count; instancing >1 is deferred
                    // (count only, doc-2 §5) — a >1 count is logged and the draw runs a
                    // single instance.
                    op::IT_NUM_INSTANCES if draw_on => index_state.set_instances(body),
                    // A DrawIndexAuto bound to a synthesizable pipeline appends its
                    // CreatePipeline (on cache miss) + vertex binds + viewport/scissor +
                    // DrawAuto to the submit's command list; a non-embedded/unbound draw is
                    // cleanly deferred. Only in Draw mode (PresentSubset ignores it).
                    op::IT_DRAW_INDEX_AUTO if draw_on => self.dispatch_draw_auto(body, &mut cmds),
                    // An indexed draw: pull the index buffer through the cache and emit
                    // DrawIndexed. Same pipeline/vertex/viewport setup as DrawAuto.
                    op::IT_DRAW_INDEX_2 if draw_on => {
                        self.dispatch_draw_index_2(body, &index_state, &mut cmds)
                    }
                    // An offset indexed draw (`sceGnmDrawIndexOffset` → this packet): same
                    // as DrawIndex2 but the packet carries no index base — the base/type
                    // come from the bound IT_INDEX_BASE/IT_INDEX_TYPE state, and the draw
                    // starts `index_offset` elements in. Deferred cleanly if no base bound.
                    op::IT_DRAW_INDEX_OFFSET_2 if draw_on => {
                        self.dispatch_draw_index_offset(body, &index_state, &mut cmds)
                    }
                    // IT_INDEX_BUFFER_SIZE records the max index count (VGT index-buffer
                    // size) for the following auto/offset draw (doc-2 §5); it only carries
                    // state, like IT_INDEX_TYPE/BASE. Draw-mode-gated.
                    op::IT_INDEX_BUFFER_SIZE if draw_on => index_state.set_max_size(body),
                    // IT_DMA_DATA performs a CP DMA. The common memory->memory variant is
                    // executed through the bounded read + SMC-observed write seam so the
                    // destination buffer a following draw consumes is populated; register /
                    // GDS / data-fill variants (Celeste's mem->register streams among them)
                    // are cleanly deferred + logged, never guess-executed. Applied in every
                    // present_sync mode: a DMA that stages geometry/index bytes must land
                    // regardless of the draw gate.
                    op::IT_DMA_DATA => unsafe { dispatch_dma_data(body) },
                    // IT_ACQUIRE_MEM (the GFX7+ successor to SURFACE_SYNC) is a memory-acquire
                    // barrier: "prior writes to [coher_base, coher_base+coher_size) are visible
                    // to following reads". We model no GPU caches, but we DO hold a
                    // guest-range -> host-buffer resource cache whose dirty tracking only observes
                    // guest CPU writes — a range the GPU itself filled (a colour buffer Celeste
                    // then samples) stays "clean" there and would serve stale bytes. So a BOUNDED
                    // acquire is routed through [`ResourceCache::invalidate_range`] — the existing
                    // "the caller already knows a write happened to this range" hook, which is
                    // exactly what an acquire packet asserts — marking every overlapping entry for
                    // re-upload. Reusing that seam rather than inventing a second invalidation path
                    // keeps its exemptions for free (imported and render-target entries are never
                    // re-uploaded from guest bytes, doc-2 §8.2/§8.5).
                    //
                    // A WHOLE-MEMORY acquire is deliberately NOT honoured — see
                    // [`acquire_mem_range`] for why. Applied in every present_sync mode: the
                    // resource cache is submit-spanning and shared, like IT_DMA_DATA's staging.
                    //
                    // MEASURED (Celeste, ~90 s run): 2368 bounded acquires, 0 entries marked —
                    // every range Celeste acquires over is a render target (1920x1088 and
                    // 1024x576 RGBA8), and `invalidate_range` exempts `is_rt` entries by design.
                    // So today this is behaviourally INERT and the packet's real effect here is
                    // being consumed correctly instead of logged as unhandled. It is wired
                    // anyway because it is the correct semantic and it is the arm that will fire
                    // the moment a guest acquires over a plain buffer (a compute-written vertex /
                    // index range) — the one case the guest-CPU dirty drain provably cannot see.
                    op::IT_ACQUIRE_MEM => {
                        if let Some((base, size)) = acquire_mem_range(body) {
                            self.resources.invalidate_range(base, size);
                        }
                    }
                    // IT_NOP is padding / a guest tag payload — decoded and silently
                    // skipped, never logged as unhandled (AC #3).
                    op::IT_NOP => {}
                    // An opcode we decode but do not yet act on. In Draw mode, log it once
                    // per distinct opcode per submit so a retail command buffer reveals
                    // exactly which packet class to implement next (a compute/geometry/tess
                    // op here is a STOP signal, not a Phase-A decode arm). Never fatal — the
                    // packet is skipped, decode continues.
                    other if draw_on => log_unhandled_opcode(other, &mut seen_unhandled),
                    _ => {}
                }
            }
        }
        // task-56 step 5: flush any queued RT readbacks (ReadbackPolicy::All only) at the tail
        // of the submit, so every producer draw has been recorded into `cmds` first. The
        // backend runs these after it records the passes (the RT is in SHADER_READ by then),
        // so their position at the list tail is correct. Empty under the default Off policy.
        for (id, addr, size, pitch, tiling) in self.pending_readbacks.drain(..) {
            cmds.push(BackendCmd::ReadbackRenderTarget {
                id,
                addr,
                size,
                pitch,
                tiling,
            });
        }
        // task-187: flush the snapshot's render-target PNG dumps, at the same point and for
        // the same reason — the backend runs them after `record_passes`, so the RT it copies
        // is the rendered one. Empty unless a capture is armed AND
        // `UNEMUPS4_SNAPSHOT_RENDER_TARGETS` is on. Fire-and-forget: nothing comes back, so
        // the submit thread never waits on the display thread here.
        for (id, path) in self.pending_rt_dumps.drain(..) {
            cmds.push(BackendCmd::DumpRenderTargetPng { id, path });
        }
        // Ship the whole submit as one list (nothing to send if no draw resolved).
        if !cmds.is_empty() {
            self.sink.run_command_list(&cmds);
        }
        if range.flip {
            // On-demand GPU state snapshot (task-185). A submit carrying a flip IS the frame
            // boundary: every draw of the frame just built has been recorded, and the next
            // frame's registers have not been written yet. This closes an armed capture
            // (writing its three files) and claims the next frame from the maintainer's
            // cross-thread request budget.
            //
            // It runs BEFORE `submit_and_flip` so a capture cannot be interleaved with the
            // display thread's work on the same frame, and it emits no backend command, does
            // no readback and forces no synchronisation — a captured frame renders exactly
            // like an uncaptured one (AC #5). When nothing is pending the entire cost is one
            // relaxed atomic load, once per frame.
            crate::snapshot::on_frame_boundary(self.state, ps4_core::clock::flip_count());
            self.sink.submit_and_flip(range.vo_handle, range.buf_idx);
        }
        // Releasing the command-buffer views. Nothing is freed on the in-place path; the
        // row is still timed so it stays comparable with the pre-task-208 profile, where
        // it was 5 ms of every flip.
        let t_free = prof.then(std::time::Instant::now);
        drop(dcb);
        drop(ccb);
        if let Some(t) = t_free {
            crate::profile::EXEC
                .packet_free_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            crate::profile::EXEC
                .packets
                .fetch_add(packet_count, Ordering::Relaxed);
        }
        if let Some(t) = t_run {
            crate::profile::EXEC
                .run_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Handle one `IT_DRAW_INDEX_AUTO` (opcode 0x2D). Body layout
    /// `[index_count, draw_initiator]`. Runs the shared draw setup (resolve VS/PS →
    /// pipeline, bind vertex buffers, set viewport/scissor) and appends a `DrawAuto`.
    /// Any clean defer (unsupported shader/target, bad V#) skips the draw, never fatal.
    fn dispatch_draw_auto(&mut self, body: &[u32], cmds: &mut Vec<BackendCmd>) {
        let vertex_count = body.first().copied().unwrap_or(0);
        if self
            .setup_draw(vertex_count, "DrawIndexAuto", cmds)
            .is_some()
        {
            cmds.push(BackendCmd::DrawAuto {
                vertex_count: rectlist_vertex_count(self.state, vertex_count),
            });
        }
    }

    /// Handle one `IT_DRAW_INDEX_2` (opcode 0x27). GFX6 body layout
    /// `[max_size, index_base_lo, index_base_hi, index_count, draw_initiator]` — the
    /// packet carries its own index base + count. Runs the shared draw setup, pulls the
    /// index buffer through the resource cache (upload-on-use), and appends `DrawIndexed`.
    /// A missing/unreadable index buffer or any setup defer skips the draw cleanly.
    fn dispatch_draw_index_2(
        &mut self,
        body: &[u32],
        index_state: &IndexState,
        cmds: &mut Vec<BackendCmd>,
    ) {
        let [max_size, base_lo, base_hi, index_count] = match body {
            [max_size, base_lo, base_hi, index_count, ..] => {
                [*max_size, *base_lo, *base_hi, *index_count]
            }
            _ => {
                tracing::debug!("[GNM] DrawIndex2 body too short; deferring draw");
                return;
            }
        };
        // VGT clamps the draw to the packet's own MAX_SIZE (body[0], `VGT_DMA_MAX_SIZE` — the
        // number of indices the bound buffer holds); a stale/oversized `index_count` would
        // otherwise fetch past the declared index buffer and read adjacent guest bytes as index
        // values. Clamp when MAX_SIZE is programmed (nonzero); an unset max (0) leaves the count
        // as-is. AMD PM4 IT_DRAW_INDEX_2 body: [max_size, index_base_lo, index_base_hi,
        // index_count, draw_initiator]. Mirrors the IT_DRAW_INDEX_OFFSET_2 clamp below.
        let index_count = if max_size != 0 {
            index_count.min(max_size)
        } else {
            index_count
        };
        // The packet's own base supersedes a running IT_INDEX_BASE; a zero base falls
        // back to the last IT_INDEX_BASE.
        let packet_base = u64::from(base_lo) | (u64::from(base_hi) << 32);
        let index_base = if packet_base != 0 {
            packet_base
        } else {
            index_state.base
        };
        let index_type = index_state.index_type;

        if self.setup_draw(index_count, "DrawIndex2", cmds).is_none() {
            return;
        }

        // Pull the index buffer through the cache (upload-on-use). A null/unmapped base
        // defers the draw cleanly (AC #3) rather than emitting a DrawIndexed over an
        // absent buffer.
        let elem_size = match index_type {
            IndexType::U16 => 2u64,
            IndexType::U32 => 4u64,
        };
        let size = elem_size.saturating_mul(u64::from(index_count));
        if index_base == 0 || size == 0 {
            tracing::debug!(
                "[GNM] DrawIndex2 has a null/empty index buffer (base={index_base:#x} \
                 count={index_count}); deferring draw"
            );
            return;
        }
        let key = ResourceKey {
            addr: index_base,
            size,
            layout: ResLayout::IndexBuf,
        };
        let id = self
            .resources
            .get(key, &crate::idmem::BoundedMem, self.dirty.as_ref(), cmds);
        cmds.push(BackendCmd::DrawIndexed {
            id,
            index_count,
            index_type,
        });
    }

    /// Handle one `IT_DRAW_INDEX_OFFSET_2` (opcode 0x35, `sceGnmDrawIndexOffset`). GFX6 body
    /// layout `[max_size, index_offset, index_count, draw_initiator]` — unlike `DRAW_INDEX_2`
    /// this packet carries **no index base**: the base + width come from the bound
    /// `IT_INDEX_BASE`/`IT_INDEX_TYPE` state, and the draw reads `index_count` indices
    /// starting `index_offset` elements into that buffer. Runs the shared draw setup, pulls
    /// the offset sub-range of the index buffer through the resource cache, and appends
    /// `DrawIndexed`. A missing/unbound index base (Celeste sets none via GNM) or any setup
    /// defer skips the draw cleanly — never fatal.
    fn dispatch_draw_index_offset(
        &mut self,
        body: &[u32],
        index_state: &IndexState,
        cmds: &mut Vec<BackendCmd>,
    ) {
        let [index_offset, packet_count] = match body {
            [_max_size, index_offset, index_count, ..] => [*index_offset, *index_count],
            _ => {
                tracing::debug!("[GNM] DrawIndexOffset body too short; deferring draw");
                return;
            }
        };
        // The offset draw carries no index-buffer size of its own; a preceding
        // IT_INDEX_BUFFER_SIZE bounds how many indices the bound buffer holds, and this draw
        // reads `count` indices starting `index_offset` elements in — so the readable bound is
        // `max_size - index_offset`, not `max_size`. Clamp to that (a draw can't read past the
        // declared buffer). Unset max → no clamp.
        let index_count = index_state.clamp_count_at_offset(packet_count, index_offset);

        if self
            .setup_draw(index_count, "DrawIndexOffset", cmds)
            .is_none()
        {
            return;
        }

        let index_type = index_state.index_type;
        let elem_size = match index_type {
            IndexType::U16 => 2u64,
            IndexType::U32 => 4u64,
        };
        // The offset draw reads the sub-range `[base + index_offset, base + index_offset +
        // index_count)`. A null base (no IT_INDEX_BASE bound) or an empty count defers
        // cleanly rather than emit a DrawIndexed over an absent buffer.
        let byte_offset = elem_size.saturating_mul(u64::from(index_offset));
        let index_base = index_state.base.saturating_add(byte_offset);
        let size = elem_size.saturating_mul(u64::from(index_count));
        if index_state.base == 0 || size == 0 {
            tracing::debug!(
                "[GNM] DrawIndexOffset has a null/empty index buffer (base={:#x} \
                 offset={index_offset} count={index_count}); deferring draw",
                index_state.base
            );
            return;
        }
        let key = ResourceKey {
            addr: index_base,
            size,
            layout: ResLayout::IndexBuf,
        };
        let id = self
            .resources
            .get(key, &crate::idmem::BoundedMem, self.dirty.as_ref(), cmds);
        cmds.push(BackendCmd::DrawIndexed {
            id,
            index_count,
            index_type,
        });
    }

    /// Shared draw setup for `DrawIndexAuto`/`DrawIndex2` (doc-2 §4/§5/§C4). Resolves the
    /// bound VS/PS through the provider chain, derives the render target + pipeline key
    /// (folding in the register-derived vertex-input layout), gets-or-mints the pipeline
    /// id (emitting `CreatePipeline` on a miss), pulls each referenced vertex buffer
    /// through the resource cache and emits `BindVertexBuffer`, then emits the dynamic
    /// viewport/scissor and `BindPipeline`. Returns `Some(id)` when the draw is ready to
    /// append its final draw command, or `None` for any clean defer (unsupported
    /// shader/target, bad descriptor) — the caller then skips the draw, never fatal.
    ///
    /// `what` names the draw kind for the defer logs. `count` is the vertex/index count,
    /// logged for diagnostics. Returns `Some(())` when the draw is set up, `None` on a
    /// clean defer.
    fn setup_draw(&mut self, count: u32, what: &str, cmds: &mut Vec<BackendCmd>) -> Option<()> {
        // Registers are the truth: derive the effective VS/PS from the SH bank
        // (`SPI_SHADER_PGM_*`) at draw time, with the embedded global route taking
        // precedence per stage (doc-2 §5). A register-programmed shader becomes a
        // `GcnBinary` ref → recompiled here through the provider chain.
        let bound = self.state.derive_bound_shaders();

        let (vs_host, ps_host) = match resolve_shader_pair(self.providers, &bound, &*self.dirty) {
            ShaderPairResolution::Resolved { vs, ps } => (vs, ps),
            ShaderPairResolution::NeedsGcn {
                stage,
                addr,
                hash,
                err,
            } => {
                // AC #3: recognized-but-unsupported real `.sb` GCN shader. Clean defer,
                // not a crash. debug! so headless oracle baselines (no draw path wired)
                // are untouched.
                tracing::debug!(
                    "[GNM] {what} bound to a non-recompilable (.sb GCN) {stage} shader at \
                     {addr:#x} — deferring draw (count={count})"
                );
                return self.defer_draw_gcn(what, count, stage, addr, hash, err);
            }
            ShaderPairResolution::Unbound => {
                tracing::debug!(
                    "[GNM] {what} with no resolvable VS/PS bound — skipping draw (count={count})"
                );
                return self.defer_draw(what, count, "unbound-shader");
            }
        };

        // Derive the render target + pipeline state from the shadow registers (doc-2 §5).
        // A programmed-but-unsupported or unregistered target is a clean defer + log.
        let mut draw = self.derive_draw_state(&bound, what, count)?;

        // RT-as-texture producer side (doc-2 §8.5, task-56): a draw into an OFFSCREEN target
        // creates a host render target keyed on its guest range so a later draw sampling the
        // same range binds the RT host-side (never detiling guest bytes the GPU wrote). Emits
        // exactly one `CreateRenderTarget` on first use (the cache is idempotent) plus the
        // producer's `SetRenderTarget` pass boundary, NEVER an upload. The videoout path
        // (kind = Videoout) is unchanged.
        //
        // The submit-spanning REGISTRATION (`state.render_targets`) is deliberately NOT committed
        // here: it is deferred to the commit point below, AFTER every clean-defer check has
        // passed. Registering it up-front would leave the range registered for a draw that then
        // defers and renders nothing — a later draw sampling the same range would match the
        // `render_targets.lookup` below and bind that never-rendered, empty/undefined host RT.
        if let ps4_core::gpu::TargetKind::Offscreen { base, size } = draw.target.kind {
            self.register_render_target(base, size, draw.target, cmds);
        }

        // Register-derived vertex ranges (doc-2 §C4): read the VS user-SGPR block, build a
        // fetch layout from the recompiled VS's descriptor bindings, and decode each V#. A
        // bad/null descriptor defers cleanly; an embedded VS has no fetch and yields an
        // empty result.
        let draw_buffers = self.derive_vertex_buffers(&vs_host);

        // task-172 Phase 2 (env-gated UNEMUPS4_DUMP_VBUF=<dir>): dump each resolved
        // vertex/SSBO buffer's CONTENT + hash keyed by ROLE — (VS/PS shader-lo12, per-flip
        // draw ordinal, stride, num_records, span) — NOT the raw address (our load base
        // differs from real HW, so the Phase-4 diff aligns on role). Supersedes the raw-
        // address UNEMUPS4_VBHASH probe.
        dump_vbuf_probe(self.state, &draw_buffers);

        // A recompiled VS that declares SSBO buffer bindings fetches its vertices through
        // storage buffers indexed by `gl_VertexIndex` (doc-2 §C4): it consumes no
        // vertex-input, so the host pipeline must be built with a descriptor set + push
        // constants rather than vertex-input attributes. An embedded VS (io: None) keeps the
        // register-derived vertex-input path unchanged. The pipeline protocol now carries N
        // vertex-fetch storage descriptors (task-153): a VS with several distinct vertex V#
        // (attr0/attr1/attr2 — interleaved or separate buffers) binds each as its own SSBO
        // stream, so no multi-stream defer.
        let ssbo = match vs_host.io.as_ref().map(|io| (io, io.buffers.as_slice())) {
            None | Some((_, [])) => None,
            Some((io, bs)) => Some((io, bs)),
        };

        // Constant-buffer path (doc-6 Entry 9): a recompiled shader that reads scalar
        // constants via `s_buffer_load` (Celeste's 4×4 transform matrix) declares a
        // `const_buffers` SSBO at set0/bind2. The recompiler emits that binding for WHICHEVER
        // stage does the load — a VS (transform matrix) OR a PS (Celeste's pixel-shader
        // constants), both at the same hardcoded set0/bind2. Resolve its V# from the
        // DECLARING stage's user-SGPR block BEFORE building the pipeline. IMPORTANT — same
        // discipline as the sampler path: if the CB is declared but its V# can't resolve, the
        // WHOLE draw defers cleanly; a pipeline built with a `const_storage` descriptor whose
        // `BindConstBuffer` never lands would leave that descriptor un-written (a validation
        // error / GPU crash).
        //
        // The pipeline protocol carries a SINGLE `const_storage` descriptor (one set0/bind2
        // slot), so a stage declaring >1 CB defers cleanly (strict-or-defer, doc-6 Entry 10),
        // and a draw where BOTH the VS and PS declare a CB also defers (they would collide on
        // the one set0/bind2 slot — the corpus/Celeste declares exactly one, in one stage, so
        // this is behavior-identical). `const_stage` records which stage owns it so the
        // pipeline layout's descriptor `stage_flags` match the declaring SPIR-V
        // (VUID-VkGraphicsPipelineCreateInfo-layout-07988, task-139).
        // TWO const-storage slots (task-174): the VS declares its CB at set0/bind2 and the
        // PS at set0/bind6 (distinct bindings from the recompiler), so a draw whose VS AND PS
        // BOTH load constants binds BOTH instead of deferring on a shared slot — the fix that
        // unblocks Celeste's title RT producers (mountain/parallax/bloom, task-171). Each
        // stage declares 0 or 1 CB; >1 in a stage defers (strict-or-defer, one const_storage
        // descriptor per stage, doc-6 Entry 10). Pick each stage's single CB decl.
        let cb_of =
            |io: Option<&ps4_gcn::IoLayout>| -> Option<Result<ps4_gcn::ConstBufferBinding, usize>> {
                match io.map(|io| io.const_buffers.as_slice()) {
                    None | Some([]) => None,
                    Some([cb]) => Some(Ok(cb.clone())),
                    Some(cbs) => Some(Err(cbs.len())),
                }
            };
        let vs_cb = match cb_of(vs_host.io.as_ref()) {
            Some(Err(n)) => {
                tracing::debug!(
                    "[GNM] {what} VS declares {n} constant buffers (>1, unsupported by the \
                     single VS const_storage descriptor) — deferring draw (count={count})"
                );
                return self.defer_draw(what, count, "vs-multiple-const-buffers");
            }
            Some(Ok(cb)) => Some(cb),
            None => None,
        };
        let mut ps_cb = match cb_of(ps_host.io.as_ref()) {
            Some(Err(n)) => {
                tracing::debug!(
                    "[GNM] {what} PS declares {n} constant buffers (>1, unsupported by the \
                     single PS const_storage descriptor) — deferring draw (count={count})"
                );
                return self.defer_draw(what, count, "ps-multiple-const-buffers");
            }
            Some(Ok(cb)) => Some(cb),
            None => None,
        };
        // Diagnostic gate (task-174 AC #1, UNEMUPS4_DUAL_CB_VS_ONLY): when BOTH stages declare
        // a CB, drop the PS-CB so only the VS-CB binds. Isolates whether the RT producers need
        // merely to RENDER (VS transform enough) vs also need their PS constant content. NOT
        // the fix — the default now binds both.
        if vs_cb.is_some() && ps_cb.is_some() && dual_cb_vs_only() {
            tracing::debug!(
                "[GNM] {what} DUAL_CB_VS_ONLY: dropping PS-CB, binding VS-CB only (count={count})"
            );
            ps_cb = None;
        }
        // Resolve each stage's inline V# from its OWN user-SGPR block BEFORE building the
        // pipeline. A declared-but-unresolvable CB defers the WHOLE draw (same discipline as
        // the sampler path — a const_storage descriptor whose BindConstBuffer never lands
        // would leave that descriptor un-written: a validation error / GPU crash).
        let vs_const = match vs_cb {
            None => None,
            Some(cb) => {
                let Some(range) = self.derive_const_buffer(cb.source, Stage::Vertex) else {
                    tracing::debug!(
                        "[GNM] {what} VS declares a constant buffer but its V# did not resolve — \
                         deferring draw (count={count})"
                    );
                    return self.defer_draw(what, count, "vs-const-buffer-unresolved");
                };
                Some((cb, range))
            }
        };
        let ps_const = match ps_cb {
            None => None,
            Some(cb) => {
                let Some(range) = self.derive_const_buffer(cb.source, Stage::Pixel) else {
                    tracing::debug!(
                        "[GNM] {what} PS declares a constant buffer but its V# did not resolve — \
                         deferring draw (count={count})"
                    );
                    return self.defer_draw(what, count, "ps-const-buffer-unresolved");
                };
                Some((cb, range))
            }
        };

        // Sampled-texture path (doc-2 §C3/§C4): a recompiled PS that declares a combined
        // image-sampler binding (io.samplers non-empty) samples a texture. Resolve the
        // T#/S# from the PS user-SGPR block through the bounded seam BEFORE building the
        // pipeline. IMPORTANT: if the PS needs a texture but the T#/S# can't resolve, the
        // whole draw defers cleanly — building a pipeline with `texture: Some` and then
        // failing to emit a `BindTexture` would leave the combined image-sampler
        // descriptor un-written (a validation error / GPU crash).
        // Resolve EVERY declared sampler from its OWN provenance (task-199). A PS declares
        // one combined image-sampler per distinct `image_sample` descriptor pair, and the
        // pairs are genuinely different resources: Celeste's distortion pass reads a
        // memory-resident T# (a displacement map fetched through a user-data pointer) to
        // perturb the UVs, then samples the SCENE through a register-resident T# and
        // exports only that. Binding them all to one descriptor made every sample read the
        // first texture, so the distortion pass exported the displacement map.
        //
        // Order is the shader's own first-sample order, which is the order the recompiler
        // allocated the bindings in, so index i here is the module's texture i.
        let sampler_bindings: Vec<ps4_gcn::SamplerBinding> = ps_host
            .io
            .as_ref()
            .map(|io| io.samplers.clone())
            .unwrap_or_default();
        let mut texture_bindings: Vec<(TextureBinding, TextureSource)> =
            Vec::with_capacity(sampler_bindings.len());
        for sb in &sampler_bindings {
            let Some(resolved) = self.derive_texture_binding(sb) else {
                tracing::debug!(
                    "[GNM] {what} PS declares a sampler at binding {} but its T#/S# did not \
                     resolve — deferring draw (count={count})",
                    sb.binding
                );
                return self.defer_draw(what, count, "texture-descriptor-unresolved");
            };
            let binding = TextureBinding {
                set: sb.set,
                binding: sb.binding,
            };
            // RT-as-texture recognition (doc-2 §8.5, task-56): if the sampled T# base names a
            // range a prior draw rendered into as an offscreen RT (exact base + full
            // containment), this is a host-side render-to-texture — bind the RT entry with
            // ZERO CreateImage/UploadImage and SKIP the macro-tile detile guard (an RT is
            // host-linear, not guest-tiled).
            let t = &resolved.texture;
            if let Some(rt) = self.state.render_targets.lookup(t.base, t.byte_span()) {
                texture_bindings.push((binding, TextureSource::RenderTarget(rt, resolved)));
            } else {
                // Plain sampled texture: linear, 1D-thin, and linear-aligned (index 8, the
                // pitch-padded font/UI atlases, task-153) all have detilers now and proceed.
                // Only genuine 2D macro-tiling (bank/pipe swizzle, index >= 9) still lacks a
                // detiler: detiling it as 1D would ship silently-wrong texels the oracle
                // can't match, so defer cleanly rather than mis-detile (task-98 AC#2).
                if ps4_core::tiling::tile_kind(t.tiling_index)
                    == ps4_core::tiling::TileKind::Macro2d
                {
                    tracing::debug!(
                        tiling_index = t.tiling_index,
                        "[GNM] {what} texture is 2D macro-tiled (no detiler) — deferring draw \
                         (count={count})"
                    );
                    return self.defer_draw(what, count, "macro-tiled-texture");
                }
                texture_bindings.push((binding, TextureSource::Plain(resolved)));
            }
        }

        // task-178 probe (env UNEMUPS4_DRAWTEX_TRACE=1, zero-cost off): per-draw, log the sampled
        // texture base + whether it resolved as a real offscreen RENDER-TARGET or fell through to
        // a PLAIN sampled texture, plus the primitive `count`. Tests the maintainer's observation
        // ("whatever loads into the texture cache flashes full-screen"): a full-screen composite
        // quad (small count) that SHOULD sample an offscreen RT but resolves PLAIN samples the
        // just-loaded texture instead → flash. Look for PLAIN + small count on the atlas/card bases.
        {
            use std::sync::OnceLock;
            // Which surface this draw writes: the videoout framebuffer, or an offscreen RT
            // named by its guest base. Without it the trace cannot tell which target a
            // full-screen fill lands in, and the producer/consumer chain stays a guess.
            let psin: Vec<u32> = (0..4)
                .map(|i| {
                    self.state
                        .ctx_regs
                        .get(crate::pm4::opcodes::context_reg::SPI_PS_INPUT_CNTL_0 + i)
                        .unwrap_or(0xFFFF_FFFF)
                })
                .collect();
            let ccc = self
                .state
                .ctx_regs
                .get(crate::pm4::opcodes::context_reg::CB_COLOR_CONTROL);
            let vp = (
                draw.viewport.x as i32,
                draw.viewport.y as i32,
                draw.viewport.width as i32,
                draw.viewport.height as i32,
            );
            let smask = self
                .state
                .ctx_regs
                .get(crate::pm4::opcodes::context_reg::CB_SHADER_MASK);
            let bctl = self
                .state
                .ctx_regs
                .get(crate::pm4::opcodes::context_reg::CB_BLEND0_CONTROL);
            let tmask = self
                .state
                .ctx_regs
                .get(crate::pm4::opcodes::context_reg::CB_TARGET_MASK);
            let tgt = match draw.target.kind {
                ps4_core::gpu::TargetKind::Offscreen { base, .. } => format!("rt:{base:#x}"),
                _ => "videoout".to_string(),
            };
            static DT: OnceLock<bool> = OnceLock::new();
            let on = *DT.get_or_init(|| {
                std::env::var("UNEMUPS4_DRAWTEX_TRACE").is_ok_and(|v| v != "0" && !v.is_empty())
            });
            if on {
                // Reports texture 0 only. This is the legacy single-texture probe; for a
                // multi-texture draw the per-texture view is the gpu-snapshot's `sampled`
                // array (and the console-vs-us `framediff` tool), not this line.
                match texture_bindings.first().map(|(_, src)| src) {
                    Some(src) => {
                        let (base, kind) = match src {
                            TextureSource::Plain(r) => (r.texture.base, "PLAIN"),
                            TextureSource::RenderTarget(rt, _) => (rt.base, "RT"),
                        };
                        // With UNEMUPS4_RT_READBACK=1 the RT's pixels are copied back into
                        // its guest range, so a composite can report the ALPHA it is about to
                        // blend with: premultiplied ONE/ONE_MINUS_SRC_ALPHA is additive at
                        // a=0 but a destructive replace at a=1.
                        //
                        // ONLY when the readback actually ran (task-181). It REFUSES on a
                        // 2D macro-tiled surface — which is every Celeste RT — because no
                        // re-tiler can express one; the grid below then samples whatever
                        // stale bytes the range already held, which is exactly how task-179
                        // read a bright bloom target as near-black. Check the
                        // `rt readback: ... REFUSING` warning before trusting these numbers.
                        // Sample each RT at ITS OWN centre. A fixed byte offset is NOT
                        // comparable across targets of different widths: at 1920 vs 1024
                        // pixels wide the same offset lands in completely different parts of
                        // the scene, which makes a brightness comparison meaningless.
                        let texel = match src {
                            TextureSource::RenderTarget(rt, _) => {
                                use ps4_core::memory::VirtualMemoryManager;
                                let (w, h) = (rt.desc.pitch as u64, rt.desc.height as u64);
                                // MEAN over a grid across the central 80% — a single texel is
                                // not comparable between stages (each writes a viewport that
                                // covers only part of its target, so the same pixel maps to a
                                // different scene position). Mean brightness is exactly what a
                                // weights-sum-to-1 blur must preserve, so comparing means tests
                                // the claim directly, without pixel correspondence.
                                let (mut acc, mut n) = ([0u64; 4], 0u64);
                                for gy in 0..16u64 {
                                    for gx in 0..16u64 {
                                        let x = w / 10 + gx * (w * 8 / 10) / 16;
                                        let y = h / 10 + gy * (h * 8 / 10) / 16;
                                        if let Ok(b) = crate::idmem::BoundedMem
                                            .read_bytes_ranged(base + (y * w + x) * 4, 4)
                                        {
                                            for c in 0..4 {
                                                acc[c] += b[c] as u64;
                                            }
                                            n += 1;
                                        }
                                    }
                                }
                                (n > 0).then(|| {
                                    (
                                        [
                                            (acc[0] / n) as u8,
                                            (acc[1] / n) as u8,
                                            (acc[2] / n) as u8,
                                            (acc[3] / n) as u8,
                                        ],
                                        w,
                                        h,
                                    )
                                })
                            }
                            _ => None,
                        };
                        let flip = ps4_core::clock::flip_count();
                        // Raw first-vertex bytes per bound vertex stream. The composite PS is
                        // "texture * vertex colour" including alpha, so the VERTEX COLOUR's
                        // alpha — not the texture's — decides whether the premultiplied blend
                        // adds the glow (a=0) or replaces the frame (a=1).
                        let vtx: Vec<(u64, u32, Vec<f32>)> = draw_buffers
                            .ranges
                            .iter()
                            .filter(|r| r.layout == ResLayout::VertexBuf)
                            .map(|r| {
                                use ps4_core::memory::VirtualMemoryManager;
                                let stride = if r.desc.stride != 0 {
                                    r.desc.stride
                                } else {
                                    24
                                };
                                // SIX vertices, not one: a full-screen quad's UVs are only
                                // meaningful as a set. If every vertex carries the same UV the
                                // blur samples one texel, and its weighted sum collapses to a
                                // constant that the radial term then shapes into the smooth
                                // gradient we see instead of a blurred scene.
                                let b = crate::idmem::BoundedMem
                                    .read_bytes_ranged(r.addr, stride as usize * 6)
                                    .unwrap_or_default();
                                let f: Vec<f32> = b
                                    .chunks_exact(4)
                                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                                    .collect();
                                (r.addr, stride, f)
                            })
                            .collect();
                        if matches!(src, TextureSource::RenderTarget(..)) {
                            tracing::info!("[DRAWTEX] VTX base={base:#x} flip={flip} {vtx:x?}");
                        }
                        // First 4 dwords of the PS constant buffer as floats. For the bloom
                        // blur these are the texel step (1/width, 1/height ~ 5e-4) that the
                        // tap offsets scale; a wrong value spreads the 5 taps across the whole
                        // image, which averages toward the dark background and dims every
                        // stage even though the weights provably sum to 1.
                        let cbf = ps_const.as_ref().and_then(|(_, r)| {
                            use ps4_core::memory::VirtualMemoryManager;
                            let b = crate::idmem::BoundedMem
                                .read_bytes_ranged(r.addr, 16)
                                .ok()?;
                            let f =
                                |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
                            Some([f(0), f(4), f(8), f(12)])
                        });
                        tracing::info!(
                            "[DRAWTEX] {kind} base={base:#x} count={count} tgt={tgt} tmask={tmask:x?} bctl={bctl:x?} smask={smask:x?} ccc={ccc:x?} psin={psin:x?} vp={vp:?} texel={texel:?} cbf={cbf:?} flip={flip}"
                        );
                        // Dump the PS of an RT->RT draw (the bloom blur stages) so it can be
                        // disassembled; windowed on UNEMUPS4_DUMP_PS = first flip to dump from,
                        // so it captures the scene under investigation, not the splash.
                        let dump_from: Option<u64> = std::env::var("UNEMUPS4_DUMP_PS")
                            .ok()
                            .and_then(|v| v.parse().ok());
                        // Dump for ANY RT-sampling draw, videoout composites included: what
                        // drives the blend is the COMPOSITE shader's exported alpha, not the
                        // sampled texel's, so the composite PS is the one that decides
                        // add-vs-replace.
                        if dump_from.is_some_and(|f| flip >= f)
                            && let Some(crate::shader::source::ShaderRef::GcnBinary {
                                addr, ..
                            }) = bound.get(crate::shader::source::Stage::Pixel)
                        {
                            use ps4_core::memory::VirtualMemoryManager;
                            use std::sync::{Mutex, OnceLock};
                            static SEEN: OnceLock<Mutex<std::collections::HashSet<u64>>> =
                                OnceLock::new();
                            let seen = SEEN.get_or_init(|| Mutex::new(Default::default()));
                            if seen.lock().map(|mut s| s.insert(addr)).unwrap_or(false)
                                && let Ok(b) =
                                    crate::idmem::BoundedMem.read_bytes_ranged(addr, 8192)
                            {
                                let path = format!("/tmp/blur_{addr:x}.bin");
                                let _ = std::fs::write(&path, &b);
                                tracing::info!(
                                    "[DRAWTEX] dumped blur PS {addr:#x} flip={flip} -> {path}"
                                );
                            }
                        }
                    }
                    // A draw whose PS declares NO sampler still records — the descriptor
                    // guard only fires for a DECLARED binding that missed the cache. It
                    // writes whatever the recompiled shader computes with no texture, and
                    // a full-screen one then overwrites the target with that (the Celeste
                    // menu's black wipe). Log the PS address so the shader can be dumped.
                    None => {
                        let addr = match bound.get(crate::shader::source::Stage::Pixel) {
                            Some(crate::shader::source::ShaderRef::GcnBinary { addr, .. }) => addr,
                            _ => 0,
                        };
                        let flip = ps4_core::clock::flip_count();
                        // A samplerless PS exports a constant colour read from its CB (the
                        // Celeste full-screen fill is `s_buffer_load_dwordx4` + `exp mrt0`).
                        // Log that RGBA: its ALPHA decides whether a later premultiplied
                        // (ONE / ONE_MINUS_SRC_ALPHA) composite of the filled target is a
                        // no-op (a=0) or wipes the destination to black (a=1).
                        // WIDE dump: 16 floats plus the range we resolved, not just the 4 the
                        // shader reads. If the value we serve differs from the guest's intent
                        // because we read at the wrong offset (or from a stale copy), the right
                        // one usually sits a few dwords away — which a 4-float window hides.
                        let rgba = ps_const.as_ref().and_then(|(_, r)| {
                            use ps4_core::memory::VirtualMemoryManager;
                            let b = crate::idmem::BoundedMem
                                .read_bytes_ranged(r.addr, 64)
                                .ok()?;
                            let f =
                                |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
                            Some((
                                r.addr,
                                r.size,
                                (0..16).map(|i| f(i * 4)).collect::<Vec<_>>(),
                            ))
                        });
                        tracing::info!(
                            "[DRAWTEX] NOSAMPLER flip={flip} count={count} tgt={tgt} tmask={tmask:x?} smask={smask:x?} ccc={ccc:x?} psin={psin:x?} vp={vp:?} bctl={bctl:x?} ps={addr:#x} io={} \
                             spirv_words={} cb_rgba={rgba:?}",
                            if ps_host.io.is_none() {
                                "none"
                            } else {
                                "no-samplers"
                            },
                            ps_host.spirv.len()
                        );
                        // Raw dump of the PS GCN code so it can be disassembled
                        // (`cargo run -p ps4-gcn --example dump_disasm -- <file>`), WINDOWED on
                        // `UNEMUPS4_DUMP_PS` = the first flip to dump from. Without the window the
                        // one-shot-per-address dump fires on the splash and yields a shader from
                        // the wrong scene entirely.
                        let dump_from: Option<u64> = std::env::var("UNEMUPS4_DUMP_PS")
                            .ok()
                            .and_then(|v| v.parse().ok());
                        if addr != 0 && dump_from.is_some_and(|f| flip >= f) {
                            use std::sync::{Mutex, OnceLock};
                            static SEEN: OnceLock<Mutex<std::collections::HashSet<u64>>> =
                                OnceLock::new();
                            let seen = SEEN.get_or_init(|| Mutex::new(Default::default()));
                            if seen.lock().map(|mut s| s.insert(addr)).unwrap_or(false) {
                                use ps4_core::memory::VirtualMemoryManager;
                                if let Ok(buf) =
                                    crate::idmem::BoundedMem.read_bytes_ranged(addr, 8192)
                                {
                                    let p = format!("/tmp/ps_{addr:x}.bin");
                                    let _ = std::fs::write(&p, &buf);
                                    tracing::info!(
                                        "[DRAWTEX] dumped PS {addr:#x} at flip={flip} \
                                         count={count} -> {p}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // task-179 EXPERIMENT KNOBS (all env-gated, all default OFF — zero effect unless set).
        // The Celeste menu loses its 3D mountain because the bloom composite REPLACES the
        // frame instead of ADDING to it, and static analysis has not settled why. These let
        // the maintainer's eyes bisect the cause: each one isolates a single hypothesis, so
        // "which knob changes the picture" localises the defect faster than more probes.
        {
            use std::sync::OnceLock;
            fn on(var: &str) -> bool {
                std::env::var(var).is_ok_and(|v| v != "0" && !v.is_empty())
            }
            // (1) Do not write ALPHA into offscreen render targets, so the bloom RTs keep the
            // alpha their transparent clear left. If this restores the mountain, hardware
            // effectively leaves that alpha alone and our storing it is the defect.
            static ALPHA_MASK: OnceLock<bool> = OnceLock::new();
            if *ALPHA_MASK.get_or_init(|| on("UNEMUPS4_X_RT_ALPHA_MASK"))
                && matches!(
                    draw.target.kind,
                    ps4_core::gpu::TargetKind::Offscreen { .. }
                )
            {
                draw.pipeline.blend.write_mask &= 0x7;
            }
            // (2) Force an ADDITIVE destination factor (ONE) on any draw that samples a render
            // target. If the mountain returns with a correct glow, the composite was meant to
            // add and the question narrows to why its source alpha is not 0.
            static ADDITIVE: OnceLock<bool> = OnceLock::new();
            if *ADDITIVE.get_or_init(|| on("UNEMUPS4_X_ADDITIVE"))
                && texture_bindings
                    .iter()
                    .any(|(_, src)| matches!(src, TextureSource::RenderTarget(..)))
            {
                draw.pipeline.blend.control =
                    (draw.pipeline.blend.control & !(0x1F << 8)) | (1 << 8);
            }
            // (3) Drop the bloom composite entirely: a draw sampling a render target SMALLER
            // than the surface it draws into (the half-res blur chain into the full-res frame).
            // The baseline "what does the frame look like without the wiper" picture.
            static SKIP_BLOOM: OnceLock<bool> = OnceLock::new();
            if *SKIP_BLOOM.get_or_init(|| on("UNEMUPS4_X_SKIP_BLOOM"))
                && let Some(rt) = texture_bindings.iter().find_map(|(_, src)| match src {
                    TextureSource::RenderTarget(rt, _) => Some(rt),
                    TextureSource::Plain(_) => None,
                })
                && rt.desc.width < draw.target.width
            {
                tracing::debug!("[X] skipping bloom composite from rt {:#x}", rt.base);
                return self.defer_draw(what, count, "x-skip-bloom-knob");
            }
        }

        // task-171 B1-vs-B2 discriminator (env `UNEMUPS4_VBUF_TRACE=1`, zero-cost off): dump each
        // vertex V#'s decoded fields, RAW guest bytes at the base, and the offset-16 UV of the
        // first vertices — tagged with the sampled T# base so a line matches a RenderDoc EID.
        vbuf_trace(
            self.state,
            &draw_buffers,
            texture_bindings.first().map(|(_, src)| match src {
                TextureSource::Plain(r) => r.texture.base,
                TextureSource::RenderTarget(rt, _) => rt.base,
            }),
        );

        let mut key = draw.pipeline;
        // Resource-signature keying (task-130 slice 6): fold the bound-descriptor
        // provenance (each declared descriptor's set/binding) into the pipeline key so a
        // draw with the SAME shader hashes but a DIFFERENT layout mints a DISTINCT
        // pipeline instead of silently reusing the wrong one. Stride is OUT of the key —
        // it flows in as a SPIR-V PUSH CONSTANT pushed per draw (task-140), so one pipeline
        // serves every stride with no re-specialization and no wrong-stride reuse.
        key.resources = ps4_core::gpu::ResourceSignature {
            // Key on stream 0's placement (the first vertex V#). For a given shader the
            // binding scheme is deterministic (stream i → a fixed binding), so stream 0's
            // slot plus the shader hash uniquely name the layout; the stream COUNT is a
            // property of the same shader and needs no separate key field (task-153).
            storage: ssbo
                .and_then(|(_, bs)| bs.first())
                .map(|b| ps4_core::gpu::ResourceSlot {
                    set: b.set,
                    binding: b.binding,
                }),
            const_storage: vs_const
                .as_ref()
                .map(|(cb, _)| ps4_core::gpu::ResourceSlot {
                    set: cb.set,
                    binding: cb.binding,
                }),
            const_storage_fragment: ps_const
                .as_ref()
                .map(|(cb, _)| ps4_core::gpu::ResourceSlot {
                    set: cb.set,
                    binding: cb.binding,
                }),
            // One slot per declared texture, in shader order (task-199), so a PS that
            // samples two textures keys to a DIFFERENT pipeline than one that samples one
            // — the set-0 layout genuinely differs.
            textures: {
                let mut slots = [None; ps4_core::gpu::MAX_PS_TEXTURES];
                for (slot, sb) in slots.iter_mut().zip(sampler_bindings.iter()) {
                    *slot = Some(ps4_core::gpu::ResourceSlot {
                        set: sb.set,
                        binding: sb.binding,
                    });
                }
                slots
            },
        };
        let (vertex_storage, push_constants): (Vec<StorageBinding>, Option<PushConstantRange>) =
            if let Some((io, bindings)) = ssbo {
                // No phantom vertex input for the SSBO path: the VS reads no vertex-input,
                // so an attribute here would be an invalid pipeline the driver faults on.
                key.vertex_layout = None;
                // The module addresses each vertex stream with a per-draw stride read from a
                // PUSH CONSTANT (task-140): the recompiler bakes only a default (16). The
                // guest V#'s REAL stride flows onto `BindStorageBuffer` below and the backend
                // pushes it — one module, any stride, no re-emit, no re-specialize, no defer,
                // and stride stays OUT of the pipeline key. `StorageBinding.stride` here is
                // the reported default only; the live value is pushed at bind. One binding per
                // distinct vertex V# stream (task-153).
                let storage = bindings
                    .iter()
                    .map(|b| StorageBinding {
                        set: b.set,
                        binding: b.binding,
                        stride: b.stride_bytes,
                    })
                    .collect();
                // The push constant carries each stream's {num_records, stride, dst_sel}; a
                // missing value silently clamps every vertex to element 0. Cover the declared
                // fields' extent (16*N bytes) so the pipeline layout's range matches what the
                // draw pushes.
                let pc = io
                    .push_constants
                    .iter()
                    .map(|f| f.offset_bytes + f.size_bytes)
                    .max()
                    .map(|end| PushConstantRange {
                        offset: 0,
                        size: end,
                    });
                (storage, pc)
            } else {
                // Embedded / vertex-input path: fold the register-derived vertex-input
                // layout into the pipeline key. Ok(Some) = a built layout; Ok(None) = the
                // embedded no-fetch path (empty vertex input); Err = an unbuildable layout
                // → defer the draw cleanly.
                key.vertex_layout = match vertex_layout_of(&draw_buffers) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::debug!(
                            "[GNM] {what} register-derived vertex-input layout is unbuildable \
                             ({e:?}) — deferring draw (count={count})"
                        );
                        return self.defer_draw(what, count, "vertex-layout-unbuildable");
                    }
                };
                (Vec::new(), None)
            };

        // Guest-side cache: get-or-mint the id for this key. On a MISS the SPIR-V crosses
        // the channel once (CreatePipeline); on a HIT only the small bind id (decision-7).
        // The two constant-buffer SSBO descriptors the recompiled shaders declare, or None
        // (task-174): the VS const at set0/bind2 (VERTEX stage_flags) and the PS const at
        // set0/bind6 (FRAGMENT stage_flags). `stride` is unused for a flat `uint[]` constant
        // buffer. A draw may declare either, both, or neither.
        let const_storage = vs_const.as_ref().map(|(cb, _)| StorageBinding {
            set: cb.set,
            binding: cb.binding,
            stride: 0,
        });
        let const_storage_fragment = ps_const.as_ref().map(|(cb, _)| StorageBinding {
            set: cb.set,
            binding: cb.binding,
            stride: 0,
        });
        // On-demand GPU state snapshot (task-185). Recorded HERE, at the one point where the
        // draw is fully resolved — target, final pipeline key (resource signature and vertex
        // layout folded in), shaders, descriptors — but has not yet been shipped. Recording
        // earlier would capture a key the cache is never queried with; recording later would
        // have to re-derive state that has already been moved into the command list.
        //
        // The `armed()` guard is a plain bool field read: with no capture pending this costs
        // one branch and nothing else — no allocation, no formatting, no memory read (AC #4).
        if self.state.snapshot.armed() {
            // Every texture the draw samples, in shader order — a multi-texture pass
            // records them ALL, so a snapshot shows which resource each sample really
            // reads instead of only the first (task-199).
            let sampled: Vec<_> = texture_bindings
                .iter()
                .map(|(b, src)| crate::snapshot::SampledInput {
                    binding: *b,
                    source: match src {
                        TextureSource::Plain(r) => {
                            crate::snapshot::SampledSource::Plain(&r.texture, &r.sampler)
                        }
                        TextureSource::RenderTarget(rt, r) => {
                            crate::snapshot::SampledSource::RenderTarget(rt, r)
                        }
                    },
                    surface: match src {
                        TextureSource::Plain(r) => Some(texture_surface_layout(&r.texture)),
                        TextureSource::RenderTarget(..) => None,
                    },
                    // What the bind below will ACTUALLY create, from the same pure helper
                    // the bind calls — so the capture cannot drift from the GPU. Recording
                    // only the guest's request is what let the RT path's hardcoded
                    // linear/repeat hide behind a faithfully-recorded NEAREST S# (task-201).
                    sampler_bound: match src {
                        TextureSource::Plain(r) | TextureSource::RenderTarget(_, r) => {
                            sampler_desc_for(Some(&r.sampler))
                        }
                    },
                })
                .collect();
            // The whole register file AS OF THIS DRAW. `registers.json` is written at frame
            // end and therefore shows only the LAST draw's value for anything the guest
            // reprograms mid-frame — which is the exact bug class task-179 was (a per-draw
            // `SPI_PS_INPUT_CNTL_n` change invisible in an end-of-frame dump). Captured here
            // and diffed against the previous draw's, so `draws.json` carries "what the guest
            // changed since the last draw" rather than a register file that lies about 22 of
            // a frame's 23 draws. Built only when armed.
            let regs = crate::snapshot::RegSnapshot::capture(self.state);
            self.state.snapshot.record_draw(crate::snapshot::DrawInput {
                kind: what,
                count,
                draw: &draw,
                key: &key,
                regs,
                vs: bound.get(Stage::Vertex),
                vs_spirv: &vs_host.spirv,
                ps: bound.get(Stage::Pixel),
                ps_spirv: &ps_host.spirv,
                buffers: &draw_buffers.ranges,
                vs_const: vs_const.as_ref().map(|(_, r)| r),
                ps_const: ps_const.as_ref().map(|(_, r)| r),
                sampled,
            });
        }

        let id = match self.pipelines.get_or_mint(key) {
            PipelineLookup::Miss(id) => {
                cmds.push(BackendCmd::CreatePipeline {
                    id,
                    vs_spirv: vs_host.spirv.clone(),
                    ps_spirv: ps_host.spirv.clone(),
                    key: Box::new(key),
                    target: draw.target,
                    vertex_storage,
                    push_constants,
                    // One combined image-sampler binding per texture the PS samples,
                    // empty when it samples nothing (task-199).
                    textures: texture_bindings.iter().map(|(b, _)| *b).collect(),
                    const_storage,
                    const_storage_fragment,
                });
                id
            }
            PipelineLookup::Hit(id) => id,
        };
        cmds.push(BackendCmd::BindPipeline { id });

        // Sampled texture: pull the guest texture through the cache (detile → upload) and
        // create the sampler, then bind the combined image-sampler. The T#/S# already
        // resolved (or the draw deferred above), so a `texture: Some` pipeline always gets
        // its matching `BindTexture` — the descriptor is never left un-written. For an
        // RT-as-texture bind the source is the host render target (no upload).
        for (binding, source) in &texture_bindings {
            match source {
                TextureSource::Plain(resolved) => self.bind_texture(*binding, resolved, cmds),
                TextureSource::RenderTarget(rt, resolved) => {
                    // The guest's OWN S# — an RT-as-texture bind substitutes the image, never
                    // the sampler (task-201).
                    self.bind_render_target_as_texture(*binding, rt, Some(&resolved.sampler), cmds);
                }
            }
        }

        // Constant buffers (doc-6 Entry 9, task-174): pull each stage's guest bytes through
        // the resource cache (upload-on-use) and bind its SSBO — the VS const at set0/bind2,
        // the PS const at set0/bind6. Each V# resolved above (or the draw deferred), so a
        // pipeline with a const_storage/const_storage_fragment descriptor always gets its
        // matching `BindConstBuffer` — neither descriptor is ever left un-written.
        for (cb, range) in vs_const.iter().chain(ps_const.iter()) {
            let cache_key = ResourceKey {
                addr: range.addr,
                size: range.size,
                layout: ResLayout::ConstBuf,
            };
            let res_id = self.resources.get(
                cache_key,
                &crate::idmem::BoundedMem,
                self.dirty.as_ref(),
                cmds,
            );
            cmds.push(BackendCmd::BindConstBuffer {
                set: cb.set,
                binding: cb.binding,
                id: res_id,
            });
        }

        if let Some((_, bindings)) = ssbo {
            // The VS fetches one SSBO stream per distinct vertex V# (task-153). Pair each
            // declared binding with its VertexBuf range (both are in fetch/declaration order),
            // pull each range's bytes through the resource cache, and bind it at its own
            // (set, binding) with its own num_records/stride/dst_sel/format pushed into its own
            // push-constant group (`pc_offset = 16*stream`). A collapse-to-one binding (the
            // pre-task-153 bug) fetched attr2's UV from attr0's buffer → the black Celeste
            // frame.
            let vertex_ranges: Vec<_> = draw_buffers
                .ranges
                .iter()
                .filter(|r| r.layout == ResLayout::VertexBuf)
                .collect();
            if vertex_ranges.len() < bindings.len() {
                tracing::debug!(
                    "[GNM] {what} SSBO VS declares {} streams but resolved only {} vertex \
                     ranges — deferring draw (count={count})",
                    bindings.len(),
                    vertex_ranges.len()
                );
                return self.defer_draw(what, count, "ssbo-stream-count-mismatch");
            }
            for (stream, (binding, range)) in bindings.iter().zip(vertex_ranges.iter()).enumerate()
            {
                // A non-16 stride NO LONGER defers (task-140): the module's stride is a PUSH
                // CONSTANT, so the fetch addresses the guest V#'s real stride dynamically once
                // the backend pushes it. A resolved V# reports its own stride; a tightly-packed
                // vec4 (stride 0) means the recompiler's default.
                let vb_stride = if range.desc.stride != 0 {
                    range.desc.stride
                } else {
                    binding.stride_bytes
                };
                // Pack the V#'s per-channel destination swizzle into word3[11:0] form (4×3
                // bits) for this stream's fetch-swizzle push constant (task-155).
                let ds = range.desc.dst_sel;
                let dst_sel_packed = (ds[0] as u32 & 0x7)
                    | ((ds[1] as u32 & 0x7) << 3)
                    | ((ds[2] as u32 & 0x7) << 6)
                    | ((ds[3] as u32 & 0x7) << 9);
                // Pack the V#'s data/number format for this stream's fetch-format push constant
                // (task-164): dfmt in [7:0], nfmt in [15:8]. The recompiled VS unpacks each
                // fetched component per this format — Celeste's `_8_8_8_8` UNORM sprite color
                // (dfmt 10 / nfmt 0) decodes one packed dword to four normalized floats instead
                // of reading four raw dwords as position-dependent garbage.
                let format_packed = range.desc.packed_format();
                let cache_key = ResourceKey {
                    addr: range.addr,
                    size: range.size,
                    layout: ResLayout::VertexBuf,
                };
                let res_id = self.resources.get(
                    cache_key,
                    &crate::idmem::BoundedMem,
                    self.dirty.as_ref(),
                    cmds,
                );
                cmds.push(BackendCmd::BindStorageBuffer {
                    set: binding.set,
                    binding: binding.binding,
                    id: res_id,
                    num_records: range.desc.num_records,
                    stride: vb_stride,
                    dst_sel: dst_sel_packed,
                    format: format_packed,
                    // This stream's push-constant group: 4 uints × 4 bytes per stream.
                    pc_offset: stream as u32 * 16,
                });
            }
        } else {
            // Bind each vertex buffer, pulling it through the resource cache (upload-on-use).
            for (slot, range) in draw_buffers
                .ranges
                .iter()
                .filter(|r| r.layout == ResLayout::VertexBuf)
                .enumerate()
            {
                let cache_key = ResourceKey {
                    addr: range.addr,
                    size: range.size,
                    layout: ResLayout::VertexBuf,
                };
                let res_id = self.resources.get(
                    cache_key,
                    &crate::idmem::BoundedMem,
                    self.dirty.as_ref(),
                    cmds,
                );
                cmds.push(BackendCmd::BindVertexBuffer {
                    slot: slot as u32,
                    id: res_id,
                    stride: range.desc.stride,
                });
            }
        }

        // RT-as-texture producer side (doc-2 §8.5, task-56): commit point for the submit-spanning
        // REGISTRATION. The draw has now cleared every clean-defer check, so record the OFFSCREEN
        // target's guest range in `state.render_targets` — a later draw sampling that range then
        // resolves to this RT host-side (the `render_targets.lookup` above). Deferred to here
        // (the `CreateRenderTarget`/`SetRenderTarget` emission stays up-front, before those
        // checks) so a draw that DEFERS after deriving an offscreen target never leaves a
        // never-rendered RT registered for a later draw to bind as an empty/undefined texture.
        if let ps4_core::gpu::TargetKind::Offscreen { base, size } = draw.target.kind {
            self.state
                .render_targets
                .register(crate::state::RegisteredRt {
                    base,
                    size,
                    desc: draw.target,
                });
        }

        // Dynamic viewport/scissor (doc-2 §5): the pipeline declares the dynamic states,
        // so the register-derived rects cross as plain data here.
        cmds.push(BackendCmd::SetViewport(viewport_rect(draw.viewport)));
        cmds.push(BackendCmd::SetScissor(scissor_rect(draw.scissor)));
        let _ = id;
        Some(())
    }

    /// Derive the register-derived vertex buffers a draw references (doc-2 §C4). Builds a
    /// [`FetchLayout`] from the recompiled VS's descriptor bindings, reads the VS
    /// user-SGPR block, and decodes each V# through the **bounded** seam (the V#
    /// pointers are register-derived and untrusted). An embedded VS (no `io`) has no
    /// fetch and yields an empty result; a bad/null descriptor is deferred inside
    /// [`derive_buffer_ranges`], never fatal.
    fn derive_vertex_buffers(&self, vs_host: &HostShader) -> DrawBuffers {
        let Some(layout) = fetch_layout_of(vs_host) else {
            return DrawBuffers::default();
        };
        if layout.buffers.is_empty() {
            return DrawBuffers::default();
        }
        let user = UserData::from_regs(self.state, Stage::Vertex);
        match bounded_read() {
            Some(reader) => derive_buffer_ranges(&user, &layout, reader.as_ref()),
            None => {
                // No bounded seam wired (headless): the untrusted V# pointers cannot be
                // read safely, so no vertex buffer is bound. The draw still runs (the
                // pipeline is valid); a real emulator always has the seam wired.
                tracing::debug!(
                    "[GNM] bounded read seam not wired; no vertex buffers bound for this draw"
                );
                DrawBuffers::default()
            }
        }
    }

    /// Resolve the VS's scalar constant-buffer range (doc-6 Entry 9) from the binding's
    /// resolved descriptor provenance (task-130). A recompiled VS's `s_buffer_load` names an
    /// SBASE SGPR quad holding a 128-bit V# *inline* (unlike the vertex path's descriptor-set
    /// pointer): the gnmx driver preloads that V# into the VS user-SGPR block. The
    /// recompiler recorded WHICH SGPR quad in [`ConstBufferBinding::source`] (an
    /// [`InlineVSharp`] whose `sgpr` is the `s_buffer_load` SBASE), so this decodes the V#
    /// directly from `s[sgpr..sgpr+4]` — no Celeste-shaped hardcoded slot, no memory read for
    /// the descriptor itself. The decoded V#'s `base`/`byte_span` give the `(addr, size)` the
    /// cache uploads. Returns `None` (a clean defer) when the source is not an inline V#, the
    /// SGPR quad runs past the 16-slot block (strict-or-defer, doc-6 Entry 10), or the V# is
    /// null/degenerate — so a `const_storage: Some` pipeline never lacks its `BindConstBuffer`.
    ///
    /// [`ConstBufferBinding::source`]: ps4_gcn::ConstBufferBinding::source
    /// [`InlineVSharp`]: ps4_gcn::DescriptorSource::InlineVSharp
    fn derive_const_buffer(
        &self,
        source: ps4_gcn::DescriptorSource,
        stage: Stage,
    ) -> Option<crate::vbuf::BufferRange> {
        use crate::vbuf::decode_v_sharp;
        // The CB V# lives inline in the SGPR quad the `s_buffer_load` SBASE named; a
        // descriptor-set pointer (SetPointer) is not the const-buffer ABI, so defer.
        let ps4_gcn::DescriptorSource::InlineVSharp { sgpr } = source else {
            tracing::debug!(
                "[GNM] {stage:?} constant-buffer source is not an inline V# ({source:?}); \
                 deferring draw"
            );
            return None;
        };
        let base = sgpr as usize;
        // Read the inline V# from the DECLARING stage's user-SGPR block — a VS const buffer
        // lives in the VS block, a PS const buffer (Celeste's pixel-shader `s_buffer_load`)
        // in the PS block. Reading the wrong block would decode a garbage V#.
        let user = UserData::from_regs(self.state, stage);
        // Read the four inline V# dwords from the SGPR quad the source names; a quad running
        // past the 16-slot block cannot be a bound CB, so defer (strict-or-defer). The V# is
        // IN the SGPRs (unlike the vertex path's descriptor-set pointer), so no bounded-seam
        // memory read here.
        let words = [
            user.slot(base)?,
            user.slot(base + 1)?,
            user.slot(base + 2)?,
            user.slot(base + 3)?,
        ];
        let desc = decode_v_sharp(words);
        if desc.is_null() {
            tracing::debug!(
                "[GNM] {stage:?} constant-buffer V# in s[{base}:{}] is null/unbound; deferring draw",
                base + 3
            );
            return None;
        }
        Some(crate::vbuf::BufferRange {
            addr: desc.base,
            size: desc.byte_span(),
            layout: ResLayout::ConstBuf,
            desc,
        })
    }

    /// Resolve the PS's sampled-texture T#/S# through the bounded seam (doc-2 §C4), reading
    /// the descriptor set from the binding's resolved provenance (task-130) instead of a
    /// Celeste-shaped hardcoded slot. The recompiler recorded WHICH user-SGPR pair points at
    /// the descriptor set in [`SamplerBinding::source`], and the S#'s SGPR block in
    /// [`SamplerBinding::s_offset`]:
    ///
    /// - [`InlineVSharp`]`{sgpr}` — the corpus shape: the T#/S# arrive directly in the user
    ///   SGPRs the launch ABI loaded. `sgpr` is the pair holding the descriptor-set pointer;
    ///   the T# sits at offset 0.
    /// - [`SetPointer`]`{sgpr, desc_offset}` — an SMRD fetched the descriptor-set pointer into
    ///   `sgpr`; the T# sits at `desc_offset` within the set.
    ///
    /// In both cases the S# byte offset within the set is derived from the SGPR distance
    /// between the S# block (`s_offset`) and the T#'s SGPR quad, scaled to bytes — for the
    /// corpus (T# `s[0:7]`, S# `s[8:11]`) that is `(8 - 0) × 4 = 32`, exactly the T# size.
    ///
    /// The T#/S# pointers are register-derived and untrusted, so the read is range-validated.
    /// Returns `None` (a clean defer) when the seam is unwired, the SGPR pair runs past the
    /// block (strict-or-defer, doc-6 Entry 10), the pointer is null, the read faults, or the
    /// T# is degenerate — the caller then defers the whole draw so a `texture: Some` pipeline
    /// never lacks its `BindTexture`.
    ///
    /// [`SamplerBinding::source`]: ps4_gcn::SamplerBinding::source
    /// [`SamplerBinding::s_offset`]: ps4_gcn::SamplerBinding::s_offset
    /// [`InlineVSharp`]: ps4_gcn::DescriptorSource::InlineVSharp
    /// [`SetPointer`]: ps4_gcn::DescriptorSource::SetPointer
    fn derive_texture_binding(
        &self,
        binding: &ps4_gcn::SamplerBinding,
    ) -> Option<TextureBindingRange> {
        let user = UserData::from_regs(self.state, Stage::Pixel);
        // Dispatch on the T#'s provenance (doc-2 §C4):
        //  * InlineVSharp — the launch ABI loaded the 256-bit T# / 128-bit S# straight into
        //    the SGPR block (Celeste's gameplay draws, and the corpus). The descriptor words
        //    ARE the SGPRs — read them directly, NO memory dereference (a dereference of the
        //    first inline T# dword as a pointer faults, task-149).
        //  * SetPointer — an SMRD fetched a descriptor-set pointer into the SGPRs; the T#/S#
        //    live in guest memory at that pointer + offset, read through the bounded seam.
        let result = match binding.source {
            ps4_gcn::DescriptorSource::InlineVSharp { sgpr } => {
                crate::vbuf::derive_texture_inline(&user, sgpr as usize, binding.s_offset as usize)
            }
            ps4_gcn::DescriptorSource::SetPointer { .. } => {
                let slot = texture_slot_of(binding)?;
                let reader = bounded_read()?;
                derive_texture(&user, &slot, reader.as_ref())
            }
        };
        match result {
            Ok(range) => Some(range),
            Err(e) => {
                tracing::debug!("[GNM] PS texture T#/S# did not resolve: {e:?}");
                None
            }
        }
    }

    /// Pull the resolved texture through the resource cache (detile → CreateImage +
    /// UploadImage on first use / after a guest write; nothing on a clean hit) and get-or-
    /// create its sampler, then bind the combined image-sampler at `binding` (doc-2 §C3/
    /// §C4). The texture cache keys on `(addr, size, ResLayout::Texture)`, so a guest write
    /// to the texel range re-uploads exactly once on next use (the dirty seam's first real
    /// texture payoff).
    fn bind_texture(
        &mut self,
        binding: TextureBinding,
        resolved: &TextureBindingRange,
        cmds: &mut Vec<BackendCmd>,
    ) {
        let t = &resolved.texture;
        // TEST (white-dummy hypothesis, task-153 follow-up): FNA/XNA SpriteBatch binds a 1x1
        // WHITE dummy for solid/vertex-color draws. In our model that T# decodes to a
        // degenerate 2x1 (w<=2 && h<=2, linear/tiling 0) whose base is UNMAPPABLE, so the
        // upload path (`get_texture` → `emit_image_upload`) faults, ships NO texels, and the
        // backend samples a BLACK 2x1 → `texel(black) × vcol = black` (the mountain vanishes).
        // The load-bearing signal is UNMAPPABILITY of the descriptor base: the degenerate
        // placeholder T# points at a garbage address, so even a small read at `base` faults
        // the bounded seam. A *valid* texture (mapped bytes) reads fine and does NOT take this
        // branch. Probe only the first few bytes — reading the full `byte_span` here (up to
        // megabytes for a real atlas) on every draw would be a large per-draw copy just to
        // test mappability; the real upload path reads the bytes it needs.
        let unmappable = {
            use ps4_core::memory::VirtualMemoryManager as _;
            let probe = (t.byte_span() as usize).min(16);
            crate::idmem::BoundedMem
                .read_bytes_ranged(t.base, probe)
                .is_err()
        };
        if unmappable {
            tracing::debug!(
                base = format_args!("{:#x}", t.base),
                width = t.width,
                height = t.height,
                tiling_index = t.tiling_index,
                "white-dummy fallback: unmappable T# → binding 1x1 white"
            );
            let image_id = self.resources.get_white_dummy(cmds);
            let sampler_id = self
                .resources
                .get_sampler(sampler_desc_for(Some(&resolved.sampler)), cmds);
            cmds.push(BackendCmd::BindTexture {
                set: binding.set,
                binding: binding.binding,
                image_id,
                sampler_id,
            });
            return;
        }
        // The detile-relevant surface description, built by the one shared helper so the
        // snapshot's texture dump (task-185) detiles with the EXACT layout the upload used.
        let surface = texture_surface_layout(t);
        if surface.tiling == Tiling::LinearGeneral
            && ps4_core::tiling::tile_kind(t.tiling_index) == ps4_core::tiling::TileKind::Macro2d
        {
            // A macro-tiled T# has no detiler and `setup_draw` filters it out before the
            // pipeline is built, so it cannot reach here — shout rather than silently
            // mis-detiling it as linear.
            tracing::error!(
                tiling_index = t.tiling_index,
                "bind_texture reached a macro-tiled T# that setup_draw should have \
                 deferred; treating as linear"
            );
        }
        if surface.tiling == Tiling::LinearAligned {
            // task-155: log the decoded T# row pitch vs the align-64 guess so a wrong guess
            // (Celeste's 1922-wide splash atlas: guessed 1984, real pitch differs → banding)
            // is visible in a smoke-loop trace.
            let guessed = ps4_core::tiling::linear_aligned_pitch(t.width);
            let used = ps4_core::tiling::linear_aligned_pitch_or(t.width, t.pitch);
            tracing::debug!(
                width = t.width,
                height = t.height,
                decoded_pitch = t.pitch,
                guessed_pitch = guessed,
                used_pitch = used,
                base = format_args!("{:#x}", t.base),
                "linear-aligned texture pitch"
            );
        }
        let key = ResourceKey {
            addr: t.base,
            size: t.byte_span(),
            layout: ResLayout::Texture {
                format: SurfaceFormat {
                    dfmt: t.dfmt,
                    nfmt: t.nfmt,
                },
                surface,
            },
        };
        // sRGB textures (T# `IMG_NUM_FORMAT_SRGB` = 9, GCN/Sea-Islands) must sample through
        // an _SRGB Vulkan format so `OpImageSample` auto-decodes the texel to LINEAR, letting
        // the fragment shader composite in linear space (task-154 residual #2). Other number
        // formats (Celeste's atlas is `nfmt = 0` UNORM) stay UNORM — decoding a non-sRGB
        // texel would wrongly darken it. Only the transfer function differs; the detiled
        // linear RGBA byte layout the upload produces is identical.
        const IMG_NUM_FORMAT_SRGB: u8 = 9;
        let texture_format = if t.nfmt == IMG_NUM_FORMAT_SRGB {
            TextureFormat::R8G8B8A8Srgb
        } else {
            TextureFormat::R8G8B8A8Unorm
        };
        let image_id = self.resources.get_texture(
            key,
            surface,
            texture_format,
            &crate::idmem::BoundedMem,
            self.dirty.as_ref(),
            cmds,
        );
        // Sampler: the filter + per-axis wrap the S# selected (decision-3 — no anisotropy/
        // mips). `CLAMP_X`/`CLAMP_Y` are honored so a wrapping texture wraps and a clamped
        // one clamps (task-173: Celeste's snow tile is WRAP, its backdrop CLAMP_EDGE).
        let desc = sampler_desc_for(Some(&resolved.sampler));
        let sampler_id = self.resources.get_sampler(desc, cmds);
        cmds.push(BackendCmd::BindTexture {
            set: binding.set,
            binding: binding.binding,
            image_id,
            sampler_id,
        });
    }

    /// Emit an offscreen render target's `CreateRenderTarget` + `SetRenderTarget` (doc-2 §8.5,
    /// task-56 RT-as-texture producer). Called for a draw whose derived target is
    /// [`TargetKind::Offscreen`]: it pulls the RT through the resource cache keyed on its
    /// `[base, base+size)` range as a [`ResLayout::RenderTarget`] — which emits exactly one
    /// `CreateRenderTarget` on first use and NEVER an upload (the GPU fills it) — and opens its
    /// pass with `SetRenderTarget`. The submit-spanning REGISTRATION into `state.render_targets`
    /// (which makes a later draw sampling the same range resolve as RT-as-texture) is NOT done
    /// here: the caller commits it only after the draw has cleared every clean-defer check, so a
    /// draw that defers never leaves a never-rendered RT registered. RGBA8 only this phase.
    fn register_render_target(
        &mut self,
        base: u64,
        size: u64,
        target: TargetDesc,
        cmds: &mut Vec<BackendCmd>,
    ) {
        let (key, surface) = render_target_key(base, size, target);
        // First use emits one CreateRenderTarget; later uses are clean hits (no command).
        let rt_id = self.resources.get_render_target(
            key,
            surface,
            ps4_core::gpu::ColorFormat::R8G8B8A8Unorm,
            cmds,
        );
        // task-56 step 4: open a render pass into this RT for the draw that follows. The
        // backend segments the submit into passes and records the next draw INTO this RT's
        // own render pass/framebuffer (color attachment), then barriers it to SHADER_READ so
        // a later videoout draw can sample it. Emitted here — right before the producer draw —
        // so the RT id is carried whether the `CreateRenderTarget` fired (first use) or was a
        // clean cache hit (no create), which is why the id cannot be inferred from the stream.
        cmds.push(BackendCmd::SetRenderTarget { id: rt_id });
        // task-56 step 5: under ReadbackPolicy::All, queue a readback of this RT into its
        // guest range. Queued (not emitted here) so it lands AFTER the producer draw the
        // caller pushes next — the readback must read a rendered RT, not an empty one. Off by
        // default: the queue stays empty and zero readback commands are ever produced.
        if self.readback_policy == ReadbackPolicy::All {
            // De-dupe per RT id so re-rendering the same base in one submit reads back once.
            if !self.pending_readbacks.iter().any(|(id, ..)| *id == rt_id) {
                // The GUEST surface geometry travels with the command (task-181). The host RT
                // image is the CONTENT extent (task-180), so the backend on its own would pack
                // rows at the content width — skewing every row of a pitch-padded surface and
                // writing linear bytes into a tiled one. `pitch`/`tiling` are what make the
                // written bytes decodable (or make the backend refuse).
                self.pending_readbacks
                    .push((rt_id, base, size, target.pitch, target.tiling));
            }
        }
        // task-187: when a snapshot capture is armed and RT dumping is on, queue a PNG of
        // this target's HOST image. Queued (not emitted here) for the same reason the
        // readback above is — it must copy a rendered RT, not an empty one.
        //
        // This is the DIAGNOSTIC path and is deliberately independent of the readback policy
        // above: it needs no guest layout, refuses nothing for a tiling reason, and writes no
        // guest memory, so it works on the 2D macro-tiled targets the readback declines. The
        // recorder returns `None` (and nothing is emitted) unless armed + opted in, and at
        // most once per target per frame.
        if let Some(path) = self.state.snapshot.request_rt_dump(base, &target) {
            self.pending_rt_dumps.push((rt_id, path));
        }
    }

    /// Bind a registered render target as the sampled combined image-sampler (doc-2 §8.5,
    /// task-56 RT-as-texture consumer). Resolves the RT's cache entry (a clean hit on the
    /// same [`ResLayout::RenderTarget`] key the producer created, so ZERO
    /// `CreateImage`/`UploadImage` and no `CreateRenderTarget` re-emit) and binds it plus a
    /// sampler at `binding`. Unlike [`Self::bind_texture`] there is no guest-byte detile: the
    /// texels live on the GPU (the producer rendered them), so the guest range is never read.
    fn bind_render_target_as_texture(
        &mut self,
        binding: TextureBinding,
        rt: &crate::state::RegisteredRt,
        sampler: Option<&crate::vbuf::SamplerState>,
        cmds: &mut Vec<BackendCmd>,
    ) -> SamplerDesc {
        let (key, surface) = render_target_key(rt.base, rt.size, rt.desc);
        // Clean hit → returns the producer's RT id with NO create/upload (the whole point).
        let image_id = self.resources.get_render_target(
            key,
            surface,
            ps4_core::gpu::ColorFormat::R8G8B8A8Unorm,
            cmds,
        );
        // The IMAGE is substituted (the host RT stands in for the guest's T#, RGBA8-only),
        // but the SAMPLER is the guest's own: an RT-as-texture composite selects its filter
        // and wrap exactly like any other sampled draw. Binding a fixed linear/repeat here
        // was a task-56 shortcut, and it bilinear-smeared Celeste's whole 320x180 -> 1080p
        // upscale (task-201). Ground truth from a console capture: the upscale draw asks for
        // NEAREST/ClampToEdge, and the bloom draws in the same frame ask for LINEAR — so the
        // S# must be honoured per draw, not replaced by any single default.
        let desc = sampler_desc_for(sampler);
        let sampler_id = self.resources.get_sampler(desc, cmds);
        cmds.push(BackendCmd::BindTexture {
            set: binding.set,
            binding: binding.binding,
            image_id,
            sampler_id,
        });
        desc
    }

    /// Derive the draw's [`DrawState`] (target + pipeline + viewport + scissor, doc-2 §5),
    /// returning `None` (a clean defer) for an unsupported/unregistered target. A draw
    /// with no color base is the embedded fullscreen corpus: it renders into the videoout
    /// target, so the default `TargetDesc` and a pipeline key over the default format are
    /// used.
    /// Record a clean draw defer and yield the `None` the caller returns.
    ///
    /// A deferred draw produces no `draws.json` record, and "the picture is missing a thing"
    /// is very often "a draw deferred" — so the snapshot has to carry the REASON, not just
    /// the absence (task-185 round 2). Before this, the reason lived only in a
    /// `tracing::debug!` line, which meant correlating two sources by hand; correlating two
    /// sources by hand is the work this tool exists to remove.
    ///
    /// `reason` is a short stable slug (`"macro-tiled-texture"`, …) so a `jq` filter over a
    /// burst capture can count defers by cause. The neighbouring `tracing::debug!` keeps the
    /// prose and the structured fields; this is deliberately the coarse, greppable key.
    ///
    /// Costs one `bool` read when no capture is armed, like every other snapshot hook.
    fn defer_draw<T>(&mut self, what: &str, count: u32, reason: &'static str) -> Option<T> {
        if self.state.snapshot.armed() {
            self.state.snapshot.record_deferred(what, count, reason);
        }
        None
    }

    /// The [`defer_draw`](Self::defer_draw) for an unsupported-`.sb`-GCN-shader defer, carrying
    /// the exact detail the snapshot names (task-195): which stage failed, the shader identity,
    /// and — for a `RecompileError` — the decoded unsupported instruction + its dword offset.
    ///
    /// The error is stringified HERE and ONLY when the snapshot is armed: `err.to_string()`
    /// (the recompiler's `Display`, the same text [`super::shader::gcn::defer_reason`] logs)
    /// allocates, so a non-armed run (headless oracle / normal play) never pays it — the
    /// structured error was moved up unformatted precisely so this gate is cheap.
    fn defer_draw_gcn<T>(
        &mut self,
        what: &str,
        count: u32,
        stage: &'static str,
        addr: u64,
        hash: u64,
        err: Option<Box<ps4_gcn::RecompileError>>,
    ) -> Option<T> {
        if self.state.snapshot.armed() {
            let instruction = err.map(|e| e.to_string());
            self.state
                .snapshot
                .record_deferred_gcn(what, count, stage, addr, hash, instruction);
        }
        None
    }

    fn derive_draw_state(
        &mut self,
        bound: &BoundShaders,
        what: &str,
        count: u32,
    ) -> Option<crate::derive::DrawState> {
        use crate::derive::{
            DrawState, TargetError, derive_draw_state, derive_pipeline, derive_scissor,
            derive_viewport,
        };
        match derive_draw_state(self.state, bound) {
            Ok(draw) => {
                tracing::debug!(
                    "[GNM] draw target {:?} pipeline {:?} viewport {:?} (count={count})",
                    draw.target,
                    draw.pipeline,
                    draw.viewport
                );
                Some(draw)
            }
            // No explicit color target: the embedded fullscreen-quad corpus renders into
            // the videoout target. Key the pipeline over the default RT format.
            Err(TargetError::NoColorBase) => {
                let target = TargetDesc::default();
                let pipeline = derive_pipeline(self.state, bound, target.format);
                Some(DrawState {
                    target,
                    pipeline,
                    viewport: derive_viewport(self.state),
                    scissor: derive_scissor(self.state),
                })
            }
            Err(TargetError::UnsupportedFormat { info }) => {
                tracing::debug!(
                    "[GNM] {what} color target has an unsupported CB_COLOR0_INFO \
                     format ({info:#x}); deferring draw (count={count})"
                );
                self.defer_draw(what, count, "unsupported-color-format")
            }
            Err(TargetError::UnregisteredTarget { base }) => {
                tracing::debug!(
                    "[GNM] {what} color base {base:#x} is not a registered display \
                     buffer (arbitrary RT out of scope); deferring draw (count={count})"
                );
                self.defer_draw(what, count, "unregistered-target")
            }
        }
    }
}

/// Where a draw's sampled combined image-sampler gets its texels (doc-2 §C3/§8.5). A plain
/// sampled texture is detiled from guest bytes and uploaded; a render target's texels live
/// on the GPU (a prior draw rendered them) and are bound host-side with no upload —
/// RT-as-texture (task-56). The recognition happens once at derivation; the bind step
/// dispatches on this so a `texture: Some` pipeline always gets exactly one matching
/// `BindTexture`, whether the source is a texture or an RT.
enum TextureSource {
    /// A guest-memory sampled texture: detile + upload through the cache.
    Plain(TextureBindingRange),
    /// A registered offscreen render target: bind its host RT entry, no upload. The guest's
    /// own T#/S# is carried alongside even though the bind ignores it: it is what the guest
    /// ASKED for (format, swizzle, extent, filter) versus what we substituted, and the two
    /// disagreeing is exactly the class of defect the snapshot exists to make visible
    /// (task-184). Nothing on the bind path reads it.
    RenderTarget(crate::state::RegisteredRt, TextureBindingRange),
}

/// The detile-relevant [`SurfaceLayout`] a sampled T# describes (doc-2 §C3).
///
/// The single source of truth for "how do these guest bytes lay out": [`Executor::bind_texture`]
/// keys the upload on it, and the GPU-state snapshot detiles its texture dump with it
/// (task-185 round 2). One helper rather than two, because a snapshot that detiled with a
/// layout the upload did not use would show a picture the frame never sampled — a
/// plausible-looking substitute, which is precisely what the snapshot exists to rule out.
///
/// Macro-tiled (2D bank/pipe, index >= 9) has no detiler and is deferred in `setup_draw`
/// before any bind; it maps to linear-general here only so the match stays total. Callers
/// that can be reached with one are expected to say so.
fn texture_surface_layout(t: &TextureDesc) -> SurfaceLayout {
    let tiling = match ps4_core::tiling::tile_kind(t.tiling_index) {
        ps4_core::tiling::TileKind::Linear => Tiling::LinearGeneral,
        ps4_core::tiling::TileKind::Thin1d => Tiling::Thin1d,
        ps4_core::tiling::TileKind::LinearAligned => Tiling::LinearAligned,
        ps4_core::tiling::TileKind::Macro2d => Tiling::LinearGeneral,
    };
    SurfaceLayout {
        texel: TexelSize::Bpp32,
        extent: Extent {
            width: t.width,
            height: t.height,
        },
        tiling,
        compression: Compression::Off,
        // The decoded T# row pitch (texels); only the linear-aligned detile consults it,
        // else it falls back to the align-64 heuristic (task-155).
        pitch: t.pitch,
    }
}

/// The host sampler a guest S# asks for, or the portable default when a draw has no S#.
///
/// The portable subset has no anisotropy and no mips (decision-3), so an S# reduces to a
/// filter plus the two per-axis wrap modes: `XY_MAG_FILTER` (word2[20]) and
/// `CLAMP_X`/`CLAMP_Y` (word0[2:0] / word0[5:3]), already decoded into [`SamplerState`] by
/// [`crate::vbuf::decode_s_sharp`].
///
/// `None` means the draw genuinely resolved no sampler descriptor; only then does the
/// portable default (linear/repeat) apply. Honouring the S# is not cosmetic: Celeste renders
/// at 320x180 and upscales to 1920x1080, so its composite chain and its final upscale ask for
/// NEAREST — filtering those bilinearly smears every pixel-art edge in the frame (task-201).
/// It is equally not a licence to force NEAREST: the same frame's bloom draws ask for LINEAR,
/// and hardware gives each draw what its own S# selected.
fn sampler_desc_for(s: Option<&crate::vbuf::SamplerState>) -> SamplerDesc {
    match s {
        Some(s) => SamplerDesc {
            mag_filter: if s.bilinear {
                SamplerFilter::Linear
            } else {
                SamplerFilter::Nearest
            },
            min_filter: if s.bilinear {
                SamplerFilter::Linear
            } else {
                SamplerFilter::Nearest
            },
            address_mode_u: s.clamp_x,
            address_mode_v: s.clamp_y,
        },
        None => SamplerDesc {
            mag_filter: SamplerFilter::Linear,
            min_filter: SamplerFilter::Linear,
            address_mode_u: SamplerAddressMode::Repeat,
            address_mode_v: SamplerAddressMode::Repeat,
        },
    }
}

/// Build the [`ResLayout::RenderTarget`] cache key + host [`SurfaceLayout`] for an offscreen
/// render target over `[base, base+size)` with `target`'s extent (doc-2 §8.5, task-56).
/// **Deterministic** in `(base, size, target)` alone — the producer (from its `TargetDesc`)
/// and a later consumer (from the registry's stored desc) build the *identical* key, so the
/// consumer's `get_render_target` is a clean hit on the producer's entry. RGBA8-only this
/// phase: a fixed RGBA8 [`SurfaceFormat`] and host-linear tiling, independent of any sampling
/// T#'s `dfmt`/`nfmt` (the RT is host-layout, not the guest surface).
fn render_target_key(base: u64, size: u64, target: TargetDesc) -> (ResourceKey, SurfaceLayout) {
    let surface = SurfaceLayout {
        texel: TexelSize::Bpp32,
        extent: Extent {
            width: target.width,
            height: target.height,
        },
        // A host render target is host-linear (the backend fills a color attachment, not a
        // guest-tiled surface), so the detile-relevant tiling is linear.
        tiling: Tiling::LinearGeneral,
        compression: Compression::Off,
        // Host-linear: pitch == width, so no decoded row pitch applies (task-155).
        pitch: 0,
    };
    // A fixed RGBA8 surface format keys the RT independent of any sampling T#: the RT is not
    // "the same bytes viewed as a texture", it is a GPU-authored host image.
    let key = ResourceKey {
        addr: base,
        size,
        layout: ResLayout::RenderTarget {
            format: SurfaceFormat {
                dfmt: RT_RGBA8_DFMT,
                nfmt: RT_RGBA8_NFMT,
            },
            surface,
        },
    };
    (key, surface)
}

/// The GCN `dfmt`/`nfmt` pair for the RGBA8 render targets this phase supports (doc-2 §8.5):
/// `COLOR_8_8_8_8` (dfmt 10) / `UNORM` (nfmt 0). Fixed constants keep the producer's and
/// consumer's [`render_target_key`] identical regardless of the sampling T#'s own format.
const RT_RGBA8_DFMT: u8 = 10;
const RT_RGBA8_NFMT: u8 = 0;

/// Index-draw state accumulated across a submit walk (doc-2 §5): the element width from
/// `IT_INDEX_TYPE`, the base from `IT_INDEX_BASE`, the instance count from
/// `IT_NUM_INSTANCES`, and the max index count from `IT_INDEX_BUFFER_SIZE`. A following
/// `IT_DRAW_INDEX_2` consumes it (its own carried base supersedes `base`). Defaults to
/// 16-bit indices, a null base, one instance, and no max (0 = unset).
#[derive(Clone, Copy, Debug, Default)]
struct IndexState {
    index_type: IndexType,
    base: u64,
    instances: u32,
    /// The `VGT_DMA_MAX_SIZE` from `IT_INDEX_BUFFER_SIZE`: the maximum number of indices
    /// the bound index buffer holds. 0 = unset (no clamp). A following auto/offset draw
    /// that carries no max of its own clamps its index count to this.
    max_size: u32,
}

impl IndexState {
    /// `IT_INDEX_TYPE` body `[vgt_index_type]`: bits [1:0] select 0=16-bit, 1=32-bit.
    fn set_type(&mut self, body: &[u32]) {
        self.index_type = match body.first().copied().unwrap_or(0) & 0x3 {
            1 => IndexType::U32,
            _ => IndexType::U16,
        };
    }

    /// `IT_INDEX_BUFFER_SIZE` body `[max_index_count]` (a single dword — the
    /// `VGT_DMA_MAX_SIZE`/index count of the bound index buffer for the next auto/offset
    /// indexed draw). Records the count; the draw arms clamp their index count to it.
    fn set_max_size(&mut self, body: &[u32]) {
        self.max_size = body.first().copied().unwrap_or(0);
    }

    /// The effective index count for an *offset* draw that carries no count clamp of its own,
    /// bounded by the recorded `IT_INDEX_BUFFER_SIZE` max (when set). The draw reads `count`
    /// indices starting `index_offset` elements into the bound buffer, which holds `max_size`
    /// indices — so only `max_size - index_offset` remain past the offset. Clamp to that
    /// (saturating: an offset at/beyond the declared size leaves 0 readable indices). An unset
    /// max (`0`) leaves the requested count unchanged. The non-offset (auto) draw path does not
    /// clamp — its count is the full vertex count with no index buffer to overrun.
    fn clamp_count_at_offset(&self, count: u32, index_offset: u32) -> u32 {
        if self.max_size == 0 {
            count
        } else {
            count.min(self.max_size.saturating_sub(index_offset))
        }
    }

    /// `IT_INDEX_BASE` body `[addr_lo, addr_hi]`: the 64-bit index buffer base.
    fn set_base(&mut self, body: &[u32]) {
        let lo = body.first().copied().unwrap_or(0);
        let hi = body.get(1).copied().unwrap_or(0);
        self.base = u64::from(lo) | (u64::from(hi) << 32);
    }

    /// `IT_NUM_INSTANCES` body `[instance_count]`. Instancing >1 is deferred (count only,
    /// doc-2 §5): a >1 count is logged; the draw still runs a single instance.
    fn set_instances(&mut self, body: &[u32]) {
        let n = body.first().copied().unwrap_or(1);
        self.instances = n;
        if n > 1 {
            tracing::debug!(
                "[GNM] NUM_INSTANCES={n} (>1) — instancing deferred; running a single instance"
            );
        }
    }
}

/// The recompiled VS's descriptor bindings as a [`FetchLayout`] (doc-2 §C4), or `None`
/// for an embedded VS (no `io`). Each `BufferBinding` becomes a [`BufferSlot`] built from
/// its resolved provenance (task-130): the vertex fetch's V# is always a
/// [`SetPointer`](ps4_gcn::DescriptorSource::SetPointer) — an SMRD `s_load` fetched the
/// descriptor-set pointer pair into the SGPRs the MUBUF named — so `user_sgpr` is that
/// SMRD's SBASE and `desc_offset` is the descriptor's byte offset within the set. No
/// Celeste-shaped hardcoded slot. A binding whose source is NOT a `SetPointer` is not the
/// vertex-fetch ABI, so the layout defers cleanly (`None`; strict-or-defer, doc-6 Entry 10).
/// Only the vertex-buffer bindings the VS declares are emitted, keyed as
/// [`ResLayout::VertexBuf`].
fn fetch_layout_of(vs_host: &HostShader) -> Option<FetchLayout> {
    let io = vs_host.io.as_ref()?;
    let mut buffers = Vec::with_capacity(io.buffers.len());
    for b in &io.buffers {
        let ps4_gcn::DescriptorSource::SetPointer { sgpr, desc_offset } = b.source else {
            tracing::debug!(
                "[GNM] VS vertex-buffer source is not a descriptor-set pointer ({:?}); \
                 deferring fetch layout",
                b.source
            );
            return None;
        };
        buffers.push(BufferSlot {
            user_sgpr: sgpr as usize,
            desc_offset: u64::from(desc_offset),
            layout: ResLayout::VertexBuf,
        });
    }
    Some(FetchLayout { buffers })
}

/// Build the [`TextureSlot`] a sampled-texture PS reaches its T#/S# through, from the
/// binding's resolved provenance (task-130). Replaces the Celeste-shaped
/// `CORPUS_TEXTURE_SLOT`: `user_sgpr` (the SGPR pair holding the descriptor-set pointer)
/// and the T#'s byte offset within the set come from [`SamplerBinding::source`], and the
/// S#'s byte offset is derived from the SGPR distance between the S# block
/// ([`SamplerBinding::s_offset`]) and the T#'s SGPR quad, scaled to bytes (4 bytes/SGPR).
///
/// - [`InlineVSharp`]`{sgpr}` — the corpus shape: the T#/S# arrive directly in the SGPRs
///   the launch ABI loaded; the T# sits at offset 0 in the set the `sgpr` pair points at.
/// - [`SetPointer`]`{sgpr, desc_offset}` — an SMRD fetched the pointer into `sgpr`; the T#
///   sits at `desc_offset` in the set.
///
/// Returns `None` (a clean defer, strict-or-defer per doc-6 Entry 10) when the S# SGPR
/// block sits BEFORE the T#'s SGPR quad — that cannot be a valid contiguous descriptor set,
/// so it is malformed rather than a partial bind.
///
/// [`SamplerBinding::source`]: ps4_gcn::SamplerBinding::source
/// [`SamplerBinding::s_offset`]: ps4_gcn::SamplerBinding::s_offset
/// [`InlineVSharp`]: ps4_gcn::DescriptorSource::InlineVSharp
/// [`SetPointer`]: ps4_gcn::DescriptorSource::SetPointer
fn texture_slot_of(binding: &ps4_gcn::SamplerBinding) -> Option<crate::vbuf::TextureSlot> {
    // Where the descriptor-set pointer lives (which user-SGPR pair) and the T#'s byte offset
    // within that set, from the T# provenance.
    let (user_sgpr, t_offset, t_sgpr) = match binding.source {
        ps4_gcn::DescriptorSource::InlineVSharp { sgpr } => (sgpr as usize, 0u64, u32::from(sgpr)),
        ps4_gcn::DescriptorSource::SetPointer { sgpr, desc_offset } => {
            (sgpr as usize, u64::from(desc_offset), u32::from(sgpr))
        }
    };
    // The S# block is named by its SGPR index (`s_offset`); its byte offset within the set is
    // the T#'s byte offset plus the SGPR distance from the T#'s quad, scaled to bytes. For the
    // corpus (T# s[0:7], S# s[8:11]) this is 0 + (8 - 0) × 4 = 32 (== T_SHARP_SIZE).
    let s_sgpr_delta = binding.s_offset.checked_sub(t_sgpr)?;
    let s_offset = t_offset + u64::from(s_sgpr_delta) * 4;
    Some(crate::vbuf::TextureSlot {
        user_sgpr,
        t_offset,
        s_offset,
    })
}

/// The vertex-input part of the pipeline key over a draw's decoded vertex buffers
/// (doc-2 §C4). Turns each vertex-buffer V# into one declared attribute + one binding,
/// carrying the per-attribute host format (from `dfmt`/`nfmt`) and the per-buffer stride
/// so the backend builds real vertex-input state, not a hardcoded vec4. A layout change
/// re-keys the pipeline.
///
/// The fetch model is one V# per attribute (the corpus ABI): attribute `i` fetches from
/// its own binding `i` at offset 0, matching the `slot as u32` the executor binds each
/// vertex buffer at.
///
/// Return: `Ok(Some(layout))` for a built layout; `Ok(None)` for the embedded no-fetch
/// path (no vertex buffer — empty vertex input, unchanged); `Err(())` — a clean defer,
/// *distinct* from an empty layout — when any attribute's `dfmt`/`nfmt` maps to no host
/// format ([`VertexFormat::Unsupported`]) or the attribute count exceeds the inline cap,
/// so a fetching VS with a bogus format never silently becomes an empty-input draw.
fn vertex_layout_of(draw_buffers: &DrawBuffers) -> Result<Option<VertexLayout>, ()> {
    let attrs = &draw_buffers.vertex_input.attributes;
    if attrs.is_empty() {
        return Ok(None);
    }
    if attrs.len() > MAX_VERTEX_ATTRIBUTES {
        tracing::debug!(
            "[GNM] vertex layout has {} attributes (> {MAX_VERTEX_ATTRIBUTES}) — deferring draw",
            attrs.len()
        );
        return Err(());
    }

    let mut layout = VertexLayout::default();
    for (i, a) in attrs.iter().enumerate() {
        let format = a.to_vertex_format();
        if format == VertexFormat::Unsupported {
            tracing::debug!(
                "[GNM] vertex attribute {i} has unsupported dfmt/nfmt \
                 ({:?}/{:?}) — deferring draw",
                a.dfmt,
                a.nfmt
            );
            return Err(());
        }
        // One V# per attribute: attribute i binds at slot i, offset 0 (the corpus ABI,
        // matching the BindVertexBuffer slot the draw path emits per vertex buffer).
        layout.attributes[i] = VertexAttr {
            location: i as u32,
            binding: i as u32,
            format,
            offset: 0,
        };
        layout.bindings[i] = VertexBinding {
            binding: i as u32,
            stride: a.stride,
        };
    }
    layout.attribute_count = attrs.len() as u32;
    layout.binding_count = attrs.len() as u32;
    Ok(Some(layout))
}

/// Map a derived [`Viewport`] to the Vulkan-free [`ViewportRect`] the backend sets
/// dynamically. The negative-height Y-flip is carried straight through `height`'s sign.
/// The vertex count to issue for a non-indexed draw, expanding a `DI_PT_RECTLIST`
/// (task-184).
///
/// GCN's rect list takes THREE vertices per rectangle — `p0`, `p1`, `p2` name three
/// corners and the hardware synthesizes the fourth as `p2 + p1 - p0`. Vulkan has no such
/// topology. [`derive_topology`](crate::derive::derive_topology) builds the pipeline as a
/// triangle STRIP, whose triangles `(v0,v1,v2)` and `(v1,v2,v3)` tile exactly that
/// parallelogram — so the draw must issue the fourth vertex, which is what this adds.
///
/// The fourth vertex comes from the vertex stream at index 3, not from the hardware's
/// corner synthesis; see [`PrimitiveTopology::TriangleStrip`] for when the two agree.
/// Celeste's fill VS derives position from `gl_VertexIndex` arithmetically, so index 3
/// lands on the synthesized corner exactly.
///
/// Applies to NON-INDEXED draws only. An indexed rect list would need the expansion in
/// the index buffer, which no observed title issues; such a draw renders as a strip over
/// its own indices — for the usual 3 indices, one triangle, the same coverage this layer
/// produced before topology was modelled at all.
fn rectlist_vertex_count(state: &crate::state::GpuState, vertex_count: u32) -> u32 {
    match crate::derive::derive_topology(state) {
        ps4_core::gpu::PrimitiveTopology::TriangleStrip if vertex_count == 3 => 4,
        _ => vertex_count,
    }
}

fn viewport_rect(v: Viewport) -> ViewportRect {
    ViewportRect {
        x: v.x,
        y: v.y,
        width: v.width,
        height: v.height,
    }
}

/// Map a derived [`Scissor`] to the Vulkan-free [`ScissorRect`] the backend sets
/// dynamically.
fn scissor_rect(s: Scissor) -> ScissorRect {
    ScissorRect {
        x: s.x,
        y: s.y,
        width: s.width,
        height: s.height,
    }
}

/// The outcome of resolving a draw's bound VS/PS pair through the provider chain. The
/// resolved shaders are boxed: a `HostShader` carries the SPIR-V `Arc` plus an
/// `Option<IoLayout>`, so keeping the `Resolved` payload behind a box keeps this
/// short-lived enum small (clippy `large_enum_variant`).
enum ShaderPairResolution {
    /// Both stages resolved to a host shader (embedded or recompiled SPIR-V) → draw.
    Resolved {
        vs: Box<HostShader>,
        ps: Box<HostShader>,
    },
    /// At least one stage is a recognized-but-unsupported ref (a `.sb` GCN binary the
    /// recompiler cannot lower) → defer to phase 4 (AC #3). Carries the detail the GPU
    /// snapshot names so the exact gap is visible without grepping the log (task-195):
    /// which stage failed, the shader identity, and the structured recompile error.
    NeedsGcn {
        /// Which HW stage's resolve failed — `"VS"` or `"PS"` (VS reported first when both).
        stage: &'static str,
        /// Guest `.sb` code address of the failing shader (hand this to a disassembler).
        addr: u64,
        /// Its identity hash — the same value `draws.json`'s `ShaderIdent.hash` carries.
        hash: u64,
        /// The structured recompile error, when the defer WAS a `RecompileError` (vs a coarse
        /// parse/stage/fetch defer, which is `None`). Boxed to keep this short-lived enum small
        /// (clippy `large_enum_variant`); formatted into the snapshot reason ONLY when armed.
        err: Option<Box<ps4_gcn::RecompileError>>,
    },
    /// A stage is unbound, or resolved to nothing the chain handles → skip.
    Unbound,
}

/// Resolve the bound VS/PS pair through the injected [`ShaderProvider`] chain
/// (doc-2 §4), carrying the resolved [`HostShader`]s (with their `Arc<[u32]>` SPIR-V)
/// out so the caller can hand the SPIR-V straight onto a `CreatePipeline` command.
///
/// Returns [`ShaderPairResolution::NeedsGcn`] the moment either stage is a
/// recognized-but-unsupported ref (the chain's `Err`, e.g. a `GcnBinary` before its
/// translator lands), so a mixed embedded+GCN bind still defers cleanly. Each stage is
/// resolved **exactly once** so a side-effecting provider (the phase-4 GCN one —
/// `parse_sb` + SPIR-V recompile) never runs twice per draw.
///
/// `dirty` is threaded to the provider so a GCN `.sb` recompile `watch`es its code range
/// at resolve time (doc-2 §8.3) — the resource cache's watch-on-insert shape, for shaders.
/// A later guest write to a watched range invalidates the recompile on the next per-submit
/// [`GcnShaderProvider::drain_dirty`], so a self-modified / reloaded shader re-recompiles.
fn resolve_shader_pair(
    provider: &dyn ShaderProvider,
    bound: &BoundShaders,
    dirty: &dyn DirtySource,
) -> ShaderPairResolution {
    let mem = IdentityMem;
    // Resolve BOTH stages exactly once (side effects — recompile, code-range watch — must
    // happen for each bound stage) before inspecting the outcomes.
    let vs = bound
        .vs
        .as_ref()
        .map(|r| provider.resolve(r, &mem, Some(dirty)));
    let ps = bound
        .ps
        .as_ref()
        .map(|r| provider.resolve(r, &mem, Some(dirty)));

    // Any recognized-but-unsupported ref in the pair → phase-4 defer, carrying the detail the
    // snapshot names (which stage, the shader identity, the structured error). Checked before
    // requiring both bound so a real-shader bind is reported as NeedsGcn, not Unbound. VS is
    // reported first when both fail — it is the earlier stage in the pipeline.
    let vs_host = match vs {
        Some(Ok(h)) => h,
        Some(Err(e)) => return needs_gcn("VS", bound.vs, e),
        None => None,
    };
    let ps_host = match ps {
        Some(Ok(h)) => h,
        Some(Err(e)) => return needs_gcn("PS", bound.ps, e),
        None => None,
    };

    // Both stages resolved to a host shader → draw. A missing bind or an `Ok(None)`
    // (no provider handled the ref) leaves the draw unbound rather than dispatching.
    match (vs_host, ps_host) {
        (Some(vs), Some(ps)) => ShaderPairResolution::Resolved {
            vs: Box::new(vs),
            ps: Box::new(ps),
        },
        _ => ShaderPairResolution::Unbound,
    }
}

/// Package a stage's clean defer into [`ShaderPairResolution::NeedsGcn`], pulling the shader
/// identity from its bound [`ShaderRef`] and moving the (unformatted) recompile error out of
/// the [`ShaderUnsupported`] so the snapshot can name the exact instruction (task-195). The
/// error is NOT stringified here — that happens at the deferral site, gated on the snapshot
/// being armed.
fn needs_gcn(
    stage: &'static str,
    r: Option<crate::shader::source::ShaderRef>,
    e: crate::shader::source::ShaderUnsupported,
) -> ShaderPairResolution {
    use crate::shader::source::ShaderRef;
    let addr = match r {
        Some(ShaderRef::GcnBinary { addr, .. }) => addr,
        _ => 0,
    };
    let hash = r.map(crate::derive::shader_hash).unwrap_or(0);
    ShaderPairResolution::NeedsGcn {
        stage,
        addr,
        hash,
        err: e.recompile_err.map(Box::new),
    }
}

/// task-172 Phase 2 (throwaway, env-gated `UNEMUPS4_DUMP_VBUF=<dir>`): dump each resolved
/// vertex/SSBO buffer's content + hash, keyed by the ROLE it plays — VS/PS shader-lo12
/// (`PGM_LO & 0xFFF`, the load-base-invariant scene fingerprint from Phase 1), a per-flip
/// draw ordinal, and the V#'s stride/num_records/span — so the real-HW (`vref`) capture and
/// ours align on role, not raw address. Off unless the env var is set.
fn dump_vbuf_probe(state: &crate::state::GpuState, draw_buffers: &DrawBuffers) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| {
        std::env::var("UNEMUPS4_DUMP_VBUF")
            .ok()
            .filter(|s| !s.is_empty())
    });
    let Some(dir) = dir else { return };
    if draw_buffers.ranges.is_empty() {
        return;
    }

    // Per-flip draw ordinal: reset the counter when the presented-flip count changes.
    static LAST_FLIP: AtomicU64 = AtomicU64::new(u64::MAX);
    static DRAW_ORD: AtomicU64 = AtomicU64::new(0);
    let flip = ps4_core::clock::flip_count();
    if LAST_FLIP.swap(flip, Ordering::Relaxed) != flip {
        DRAW_ORD.store(0, Ordering::Relaxed);
    }
    let ord = DRAW_ORD.fetch_add(1, Ordering::Relaxed);

    // task-178: optional flip-window filter so a run reaching the frame-1713 corrupt scene
    // dumps ONLY that window (env `UNEMUPS4_DUMP_VBUF_MIN`/`_MAX`, inclusive) instead of all
    // ~1700 flips. Unset → dump every flip (unchanged). Cheap parse, resolved once.
    static WINDOW: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    let (fmin, fmax) = *WINDOW.get_or_init(|| {
        let g = |k: &str, d: u64| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(d)
        };
        (
            g("UNEMUPS4_DUMP_VBUF_MIN", 0),
            g("UNEMUPS4_DUMP_VBUF_MAX", u64::MAX),
        )
    });
    if flip < fmin || flip > fmax {
        return;
    }

    use crate::pm4::opcodes::sh_reg;
    let lo12 = |r: u32| state.sh_regs.get(r).unwrap_or(0) & 0xFFF;
    let vs12 = lo12(sh_reg::SPI_SHADER_PGM_LO_VS);
    let ps12 = lo12(sh_reg::SPI_SHADER_PGM_LO_PS);

    let Some(src) = bounded_read() else { return };
    let _ = std::fs::create_dir_all(dir);
    for (i, r) in draw_buffers.ranges.iter().enumerate() {
        let span = r.size.min(1 << 20) as usize; // cap the dump at 1 MiB
        let Ok(bytes) = src.read_ranged(r.addr, span) else {
            continue;
        };
        let mut h = DefaultHasher::new();
        bytes.hash(&mut h);
        // Role-keyed name (no raw address): flip, draw ordinal, slot, shaders, V# shape.
        let name = format!(
            "flip{flip:06}_draw{ord}_slot{i}_vs{vs12:03x}_ps{ps12:03x}_st{}_nr{}_span{}.bin",
            r.desc.stride, r.desc.num_records, span
        );
        let _ = std::fs::write(std::path::Path::new(dir).join(&name), &bytes);
        tracing::info!(
            "[DUMP_VBUF] flip={flip} draw={ord} slot={i} vs12={vs12:#05x} ps12={ps12:#05x} \
             stride={} num_records={} span={} hash={:016x}",
            r.desc.stride,
            r.desc.num_records,
            span,
            h.finish()
        );
    }
}

/// task-171 (throwaway, env-gated `UNEMUPS4_VBUF_TRACE=1`): the B1-vs-B2 discriminator for
/// Celeste's atlas-splatter. For each draw's resolved vertex V#, log — inline, no files — the
/// decoded descriptor {base, stride, num_records, dfmt/nfmt, dst_sel}, the RAW guest bytes at
/// the V# base (first record, hex), and the decoded UV field (offset 16, stride 24) of the
/// first four vertices as f32. A splatter draw prints `uv=(0,0)(1,0)(0,1)(1,1)` (whole-atlas
/// corners); a correct draw prints fractional sub-rect UVs. Because our executor uploads the
/// vertex SSBO VERBATIM from the V# base (no repack — see `derive_buffer_ranges` →
/// `ResourceCache::get`/`emit_upload`), the raw bytes shown here ARE what Vulkan/RenderDoc
/// sees: corners in guest memory ⇒ B2 (guest emitted them); fractional in guest memory but
/// corners in Vulkan would be B1 (impossible on this verbatim path). Tagged with the VS/PS
/// PGM_LO low-12 (the load-base-invariant scene fingerprint, matching `dump_vbuf_probe`) and
/// the sampled T# base, so a trace line lines up with a RenderDoc EID. Off unless the env var
/// is set (zero-cost default).
fn vbuf_trace(state: &crate::state::GpuState, draw_buffers: &DrawBuffers, tex_base: Option<u64>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let on = ON.get_or_init(|| {
        std::env::var("UNEMUPS4_VBUF_TRACE")
            .map(|s| !s.is_empty() && s != "0")
            .unwrap_or(false)
    });
    if !on || draw_buffers.ranges.is_empty() {
        return;
    }

    use crate::pm4::opcodes::sh_reg;
    let lo12 = |r: u32| state.sh_regs.get(r).unwrap_or(0) & 0xFFF;
    let vs12 = lo12(sh_reg::SPI_SHADER_PGM_LO_VS);
    let ps12 = lo12(sh_reg::SPI_SHADER_PGM_LO_PS);
    let flip = ps4_core::clock::flip_count();
    let tex = tex_base
        .map(|b| format!("{b:#x}"))
        .unwrap_or_else(|| "none".into());

    let Some(src) = bounded_read() else {
        tracing::info!(
            "[VBUF_TRACE] flip={flip} vs12={vs12:#05x} ps12={ps12:#05x} no bounded seam"
        );
        return;
    };

    for (i, r) in draw_buffers
        .ranges
        .iter()
        .filter(|r| r.layout == ResLayout::VertexBuf)
        .enumerate()
    {
        // Record stride: the V#'s own stride, else the Celeste UI vertex record (24 bytes:
        // {posX f32, posY f32, z f32, color u32, u f32, v f32} — UV at offset 16).
        let stride = if r.desc.stride != 0 {
            r.desc.stride as usize
        } else {
            24
        };
        // Read up to the first four records, capped, through the bounded (untrusted) seam.
        let want = (stride * 4)
            .min(r.size as usize)
            .min(4096)
            .max(stride.min(64));
        let Ok(bytes) = src.read_ranged(r.addr, want) else {
            tracing::info!(
                "[VBUF_TRACE] flip={flip} slot={i} vs12={vs12:#05x} ps12={ps12:#05x} tex={tex} \
                 base={:#x} stride={} num_records={} dfmt={:?} nfmt={:?} dst_sel={:?} UNREADABLE",
                r.addr,
                r.desc.stride,
                r.desc.num_records,
                r.desc.dfmt,
                r.desc.nfmt,
                r.desc.dst_sel
            );
            continue;
        };
        let f32_at = |off: usize| -> Option<f32> {
            bytes
                .get(off..off + 4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        // Decode UV (offset 16 within each record) for the first four vertices.
        let mut uv = String::new();
        for v in 0..4 {
            match (f32_at(v * stride + 16), f32_at(v * stride + 20)) {
                (Some(u), Some(w)) => uv.push_str(&format!("({u:.4},{w:.4})")),
                _ => break,
            }
        }
        // Raw first-record bytes (hex) — guest MEMORY at the V# base: the B1/B2 oracle.
        let raw: String = bytes
            .iter()
            .take(stride.min(bytes.len()))
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(
            "[VBUF_TRACE] flip={flip} slot={i} vs12={vs12:#05x} ps12={ps12:#05x} tex={tex} \
             base={:#x} stride={} num_records={} dfmt={:?} nfmt={:?} dst_sel={:?} uv={uv} \
             raw0=[{raw}]",
            r.addr,
            r.desc.stride,
            r.desc.num_records,
            r.desc.dfmt,
            r.desc.nfmt,
            r.desc.dst_sel
        );
    }
}

/// Parse the `.sb` (OrbShdr) shader whose GCN code starts at the guest-supplied
/// `code_start` (derived from `SPI_SHADER_PGM_LO/HI`) through the **process-global
/// bounded read seam** ([`bounded_read`]) rather than a bare identity view.
///
/// `code_start` is register-derived and therefore **untrusted**: a garbage / near-unmapped
/// address handed to [`parse_sb`] over an unbounded reader would let its magic scan walk up
/// to 1 MiB of raw host memory (a SIGSEGV, or a leak of adjacent host memory into shader
/// state). Routing through the seam means every read the parser issues is range-validated
/// against the live VMA set, so a straddling / unmapped read is a clean `Err` and the scan
/// stops at the mapping boundary.
///
/// **Headless degradation** (mirrors `libSceGnmDriver::read_reg_block`): when no source is
/// wired (unit tests, no VM at boot) the seam yields `None`. Rather than fall back to an
/// unbounded identity read of an untrusted pointer, the parse is rejected cleanly with
/// [`SbParseError::MemoryFault`] — no shader, no over-read. In the real emulator the seam is
/// always wired, so the untrusted path is always bounds-checked.
///
/// This is the seam the draw path resolves a register-derived `GcnBinary` shader through;
/// the GCN decode/recompile that consumes the returned [`SbShader`] is deferred to phase 4.
#[allow(dead_code)] // wired by the phase-4 draw path; here so it cannot reach for IdentityMem
fn parse_sb_bounded(code_start: u64) -> Result<SbShader, SbParseError> {
    match bounded_read() {
        Some(src) => parse_sb(code_start, src.as_ref()),
        None => {
            // No wired seam → refuse to read an untrusted pointer unbounded. A clean fault,
            // not a fall-through to IdentityMem (which would reintroduce the over-read).
            // Signal the ABSENT seam distinctly from a genuine unmapped-address fault: a
            // harness that forgot to wire `register_bounded_read` otherwise sees only a
            // MemoryFault on every parse with no clue the seam was never installed.
            tracing::warn!(
                "[GNM] bounded read seam not wired; shader parse cannot proceed \
                 (code_start={code_start:#x}) — a real MemoryFault means an unmapped \
                 address, this means register_bounded_read was never called"
            );
            Err(SbParseError::MemoryFault)
        }
    }
}

/// Log an unhandled PM4 opcode once per distinct value (tracked in `seen`), naming it when
/// the opcode table knows it (doc-6 pull-driven instrumentation). `info!` so it surfaces in
/// a `RUST_LOG=info` retail run; a repeat of an already-seen opcode is silent.
fn log_unhandled_opcode(opcode: u8, seen: &mut Vec<u8>) {
    if seen.contains(&opcode) {
        return;
    }
    seen.push(opcode);
    match crate::pm4::opcodes::name(opcode) {
        Some(name) => tracing::info!("[GNM] unhandled PM4 opcode {opcode:#04x} ({name})"),
        None => tracing::info!("[GNM] unhandled PM4 opcode {opcode:#04x} (unknown)"),
    }
}

/// Execute one `IT_DMA_DATA` (opcode 0x50) CP DMA. GFX6 body layout (6 dwords):
/// `[engine_control, src_addr_lo, src_addr_hi, dst_addr_lo, dst_addr_hi, command]`.
///
/// The **address space** of each side is selected by the `command` word's `SAS`/`DAS`
/// bits (bit 26 = src, bit 27 = dst): `0` = a byte memory address, `1` = a register/GDS
/// address. Only the **memory→memory** variant (`SAS=0, DAS=0`) is executed here: it
/// copies `BYTE_COUNT` (command bits [20:0]) bytes from `src` to `dst`. Every other
/// variant — a register/GDS source or destination — is a CP engine feature we do not
/// model, so it is cleanly deferred + logged rather than guess-executed (writing memory
/// at a register-space "address" would corrupt an unrelated buffer). Celeste's DMA_DATA
/// stream is uniformly memory→register (`DAS=1`), so it takes the defer path.
///
/// The copy routes strictly through the **bounded seams**: the source is read through the
/// process-global [`bounded_read`] (range-validated against the live VMA set, so an
/// untrusted `src` cannot over-read host memory), and the destination is written through
/// [`write_guest`](ps4_core::write_guest::write_guest) — the SMC-observed `write_bytes`
/// path, NEVER a raw [`IdentityMem`] store (so a later-executed page a DMA wrote does not
/// leave a stale JIT translation). Headless (no seam wired) is a clean no-op. A zero src
/// or dst, or a zero byte count, is a no-op.
///
/// # Safety
/// Same identity-mapping contract as the rest of the executor: the addresses come from
/// the guest's own PM4 stream. The bounded seams provide the actual range validation;
/// this function itself performs no unchecked dereference.
unsafe fn dispatch_dma_data(body: &[u32]) {
    let [src_lo, src_hi, dst_lo, dst_hi, command] = match body {
        [_engine, src_lo, src_hi, dst_lo, dst_hi, command, ..] => {
            [*src_lo, *src_hi, *dst_lo, *dst_hi, *command]
        }
        _ => {
            tracing::debug!(
                "[GNM] DMA_DATA body too short ({} dwords); skipping",
                body.len()
            );
            return;
        }
    };

    // Command word fields (GFX6): BYTE_COUNT[20:0], SAS[26] (src addr space), DAS[27]
    // (dst addr space); 0 = memory byte address, 1 = register/GDS.
    let byte_count = (command & 0x1F_FFFF) as usize;
    let src_is_reg = (command >> 26) & 1 != 0;
    let dst_is_reg = (command >> 27) & 1 != 0;

    if src_is_reg || dst_is_reg {
        // A register/GDS source or destination is a CP feature we do not model. Defer
        // cleanly — writing memory at a register-space address would corrupt an unrelated
        // guest buffer (doc-4 "unbounded guest write" taxonomy).
        tracing::debug!(
            "[GNM] DMA_DATA is not memory->memory (src_is_reg={src_is_reg} \
             dst_is_reg={dst_is_reg}, command={command:#010x}); deferring"
        );
        return;
    }

    let src = (src_lo as u64) | ((src_hi as u64) << 32);
    let dst = (dst_lo as u64) | ((dst_hi as u64) << 32);
    if src == 0 || dst == 0 || byte_count == 0 {
        return;
    }

    // Read the source through the bounded seam (range-validated) and write the destination
    // through the SMC-observed write seam. Both degrade cleanly (None / Err) so a headless
    // run or an unmapped address is a no-op, never a host memory over-read/over-write.
    let (Some(reader), Some(writer)) = (bounded_read(), ps4_core::write_guest::write_guest())
    else {
        tracing::debug!(
            "[GNM] DMA_DATA memory->memory (src={src:#x} dst={dst:#x} bytes={byte_count}) \
             but the bounded read/write seam is not wired; skipping"
        );
        return;
    };
    let data = match reader.read_ranged(src, byte_count) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(
                "[GNM] DMA_DATA source read failed (src={src:#x} bytes={byte_count}): {e}; skipping"
            );
            return;
        }
    };
    if let Err(e) = writer.write_bytes(dst, &data) {
        tracing::debug!(
            "[GNM] DMA_DATA destination write failed (dst={dst:#x} bytes={byte_count}): {e}; \
             skipping"
        );
    }
}

/// Decode the coherency range an `IT_ACQUIRE_MEM` (opcode 0x58) packet names.
///
/// GFX7+ body layout (6 dwords):
/// `[coher_cntl, coher_size, coher_size_hi, coher_base, coher_base_hi, poll_interval]`
/// — Mesa emits exactly this order in `src/amd/common/ac_cmdbuf_cp.c:433-439`
/// (`PKT3(PKT3_ACQUIRE_MEM, 5, 0)`, each dword commented with its register), and
/// `src/amd/common/ac_parse_ib.c:382-393` dumps the older `PKT3_SURFACE_SYNC` as
/// `CP_COHER_CNTL / CP_COHER_SIZE / CP_COHER_BASE / POLL_INTERVAL` — ACQUIRE_MEM is that
/// packet plus the two `_HI` dwords.
///
/// `CP_COHER_SIZE`/`CP_COHER_BASE` count **256-byte units**, not bytes. Mesa spells this in
/// the field names — `S_030230_COHER_SIZE_HI_256B` / `S_0301E4_COHER_BASE_HI_256B` (sid,
/// `R_030230_CP_COHER_SIZE_HI` / `R_0301E4_CP_COHER_BASE_HI`), whose `_HI` halves are 8 bits
/// wide, giving a 40-bit unit index → 48-bit byte address. Real hardware confirms the shift:
/// Celeste's captured DCBs carry `coher_base=0x02bd4e00` → `0x2bd4e0000`, landing in the same
/// direct-memory region as the capture's buffer addresses.
///
/// `coher_cntl` (`R_0301F0_CP_COHER_CNTL`) selects WHICH caches to act on
/// (`CB_ACTION_ENA` bit 25, `DB_ACTION_ENA` 26, `TC_ACTION_ENA` 23, `TCL1_ACTION_ENA` 22,
/// `SH_KCACHE_ACTION_ENA` 27, `SH_ICACHE_ACTION_ENA` 29, `TC_WB_ACTION_ENA` 18, plus the
/// `CB0-7/DB_DEST_BASE_ENA` bits 6-14 that scope the range to a bound target). We model no
/// GPU cache hierarchy, so no action bit changes what we do — the RANGE is the whole of the
/// signal we can act on, and `coher_cntl` is decoded only to recognise a no-op acquire.
///
/// Returns the byte range `[base, base + size)` only when it is **bounded**. `None` for:
/// - a short body (a malformed packet is skipped, never guessed at),
/// - a zero coherency size (names no memory),
/// - a **whole-memory** acquire (`coher_size` saturated to `0xFFFF_FFFF`, the value Mesa
///   itself emits for "flush everything"). Real hardware emits exactly one of these per frame
///   as the DCB preamble (3000 of them across the captured Celeste oracle, one per frame);
///   honouring it would mark every cached entry dirty once a frame and re-upload the entire
///   working set for no correctness gain, since guest CPU writes are already covered by the
///   per-submit dirty drain. Bounded acquires carry the signal the drain CANNOT see
///   (GPU-written ranges), so those are the ones worth acting on. Our own submit path does
///   not currently surface the preamble acquire at all (0 observed in a ~90 s run), so this
///   guard is defensive — it is sized against what hardware really emits, not what we see.
fn acquire_mem_range(body: &[u32]) -> Option<(u64, u64)> {
    let [coher_cntl, size_lo, size_hi, base_lo, base_hi] = match body {
        [cntl, size_lo, size_hi, base_lo, base_hi, _poll, ..] => {
            [*cntl, *size_lo, *size_hi, *base_lo, *base_hi]
        }
        _ => {
            tracing::debug!(
                "[GNM] ACQUIRE_MEM body too short ({} dwords); skipping",
                body.len()
            );
            return None;
        }
    };
    // An acquire with no action bit set asks for nothing; nothing to invalidate.
    if coher_cntl == 0 {
        return None;
    }
    if size_lo == u32::MAX {
        // Whole-memory acquire — decoded and consumed, deliberately not applied.
        tracing::trace!("[GNM] ACQUIRE_MEM whole-memory (coher_cntl={coher_cntl:#010x}); no-op");
        return None;
    }
    // 256-byte units; the `_HI` halves are 8 bits wide on GFX7/8 (the PS4's GCN generation).
    let size = ((((size_hi & 0xFF) as u64) << 32) | size_lo as u64) * 256;
    let base = ((((base_hi & 0xFF) as u64) << 32) | base_lo as u64) * 256;
    if size == 0 {
        return None;
    }
    tracing::trace!(
        "[GNM] ACQUIRE_MEM bounded range base={base:#x} size={size:#x} \
         (coher_cntl={coher_cntl:#010x})"
    );
    Some((base, size))
}

/// The 64-bit destination address an EOP/EOS packet writes its label to, assembled
/// from the low dword and the low 16 bits of the address-hi dword (GFX6 layout).
fn label_addr(addr_lo: u32, addr_hi_word: u32) -> u64 {
    (addr_lo as u64) | (((addr_hi_word & 0xFFFF) as u64) << 32)
}

/// Write a value to an identity-mapped guest label address (guest ptr == host ptr,
/// doc-2 §1) — the CPU-visible mirror of the GPU timeline (doc-2 §C2). A zero
/// address is ignored (an EOP that only signals an interrupt names no memory).
///
/// The label address is **register/packet-derived and untrusted** (an EOP/EOS packet's
/// `addr_lo`/`addr_hi` fields). An unbounded identity store to a bad/near-unmapped
/// address would corrupt or over-write raw host memory (a SIGSEGV, doc-4 taxonomy
/// "unbounded guest write"). So the address is first **range-validated against the live
/// VMA set** through the process-global bounded seam (an 8-byte bounded read at `addr`
/// proves the label slot is inside one mapped, guest-owned region); the identity store
/// runs only when that check passes. A bad address is a clean no-op — a guest that then
/// waits on the label stalls (visible), never a host memory corruption. When no seam is
/// wired (headless), the write is skipped (the executor's present path is display-driven,
/// so there is no guest waiter to unblock there anyway).
///
/// `width_bytes` is the label width the packet declared: 4 for a 32-bit EOS datum, 8 for a
/// 64-bit EOP fence. Exactly that many bytes are validated and stored — a 32-bit label is a
/// `u32` store so it never zeroes an adjacent 4-byte fence, and its bounded-read check is 4
/// bytes wide so a 32-bit label at a mapping tail (next page unmapped) still lands where real
/// hardware would have written it.
///
/// # Safety
/// `addr`, when it passes the bounded-seam check, names a writable guest label location;
/// the identity-mapped guest range lives for the whole run, matching how the decoder reads
/// command buffers.
unsafe fn write_label(addr: u64, value: u64, width_bytes: usize) {
    if addr == 0 {
        return;
    }
    // Range-validate the untrusted label address against the mapped VMA set before the
    // identity store: a bounded read of exactly the label's width fails cleanly (Err) if the
    // range is unmapped / straddles a boundary, turning what would be an unbounded over-write
    // into a no-op. Headless (no seam) also skips — no guest waiter to unblock there.
    match bounded_read() {
        Some(reader) if reader.read_ranged(addr, width_bytes).is_ok() => {
            // Identity-mapped store (guest ptr == host ptr): the slot is proven mapped. Store
            // exactly the declared width so a 32-bit EOS label leaves the neighbouring bytes
            // untouched (a u64 store would zero [addr+4, addr+8), resetting an adjacent fence).
            if width_bytes <= 4 {
                let _ = IdentityMem.write::<u32>(addr, value as u32);
            } else {
                let _ = IdentityMem.write::<u64>(addr, value);
            }
        }
        Some(_) => {
            tracing::debug!(
                "[GNM] EOP/EOS label address {addr:#x} is not in a mapped guest range; \
                 skipping the label write (a waiter on it will stall)"
            );
        }
        None => {}
    }
}

/// Surface an EOP/EOS GPU-completion label to the guest (task-157).
///
/// Real hardware writes this label ASYNCHRONOUSLY, several ms after the submit — the GPU is
/// perpetually ~1-2 frames behind the CPU. Our executor is SYNCHRONOUS (doc-2 §C2): `run` (and
/// thus this label write) finishes before the submit HLE call returns, so the guest sees the
/// per-buffer GPU work as *already complete* the instant it resumes. The Sony gnmx SDK CPU-polls
/// this exact label to recycle its double-buffered command contexts ("is the GPU done with this
/// buffer, may I refill it without re-initializing it?"). With our instant completion it always
/// answers "yes, already done" and takes a fast recycle path that SKIPS re-emitting the buffer's
/// per-draw state — including the 8-dword texture binds (`SET_SH_REG 0x2c0c` T# + `0x2c14` S#).
///
/// That is the Celeste steady-state logo bug (task-157): the atlas T# binds emit for the first
/// two frames (one per double-buffer slot) then vanish (`3,3,0,0,0…`), so the logo renders as a
/// white bar. Deferring / withholding this label makes gnmx see the buffer as still in flight and
/// re-record the FULL state every frame — reproducing real hardware's every-frame `3,3,3,…` binds
/// and the correctly textured splash (verified by the DCB decode against `data/celeste-real-dcb/`
/// AND the `UNEMUPS4_DUMP_PNG` visual oracle). No fixed finite latency sustains it: at steady
/// 60 Hz the reused buffer's fence is always old enough to read "done" by the recycle check, so
/// the collapse merely shifts out by the latency; only leaving the fence perpetually in-flight
/// keeps gnmx on its always-correct re-record path.
///
/// This is SAFE because the guest's real frame-sync GPU-completion signal is the EQUEUE event it
/// registers with `sceGnmAddEqEvent` and blocks on in `sceKernelWaitEqueue` — which we still
/// signal on submit-done (`equeue::signal_gpu_completion`, doc-6 Entry 2). The raw EOP *memory*
/// label is, for the guest's correctness, only gnmx's buffer-recycle hint; not writing it never
/// deadlocks the guest (confirmed: Celeste runs to a live, animating, fully-textured splash).
///
/// `UNEMUPS4_GPU_EOP_SYNC=1` restores the old synchronous inline write (instant completion) for
/// A/B comparison, and for the unlikely title that CPU-polls this memory label as its only
/// GPU-completion signal (no equeue) and would otherwise not observe completion.
/// The EOP/EOS labels we WITHHELD, keyed by address, remembered so the watchdog below can
/// write them once the title proves it polls. A poller submits once and then spins on a label,
/// so there is no later submit whose `emit_label` would revisit the decision — the withheld
/// write has to be flushed from the outside, or the title hangs forever on a label that never
/// comes. Kept per-address (not a single slot) because a title may wait on SEVERAL distinct
/// labels before it blocks; a single slot would drop all but the most recent and leave a poller
/// spinning on a dropped one forever. Each entry is `(addr, value, width_bytes)` so the flush
/// stores exactly what the packet declared (EOS = 4, EOP = 8). A small `Vec` (const-init,
/// replaced per address) — a title emits only a handful of distinct completion labels.
static PENDING_EOP_LABELS: std::sync::Mutex<Vec<(u64, u64, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// Whether the flush watchdog thread has been spawned. One is enough for the whole run.
static LABEL_WATCHDOG_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn emit_label(addr: u64, value: u64, width_bytes: usize) {
    // Two titles need OPPOSITE things from this packet, and the working one must never be
    // put at risk: an equeue title (Celeste) collapses to white textures if this label is
    // written even for its first few boot frames (task-157); a label-polling title (Little
    // Nightmares) hangs if it is withheld. The default is therefore WITHHOLD, and the write
    // happens only once a title has positively shown it polls — no equeue completion ever
    // collected, past a short boot grace. `ps4_core::gpu::should_write_completion_label`
    // carries the full reasoning and the trap the first (wait-gated) attempt fell into.
    ps4_core::gpu::note_eop_submit();
    if eop_sync_completion() || ps4_core::gpu::should_write_completion_label() {
        unsafe { write_label(addr, value, width_bytes) };
        // This label is now written; drop any withheld copy of THIS address so the watchdog
        // does not redundantly re-write it. Other addresses stay pending for the watchdog.
        if let Ok(mut p) = PENDING_EOP_LABELS.lock() {
            p.retain(|&(a, _, _)| a != addr);
        }
        return;
    }
    // Withheld. Equeue title: leaving the label in its guest-initialized "pending" state keeps
    // gnmx re-recording per-draw state every frame (real HW's async completion). But a POLLER
    // submits once and spins here — no later `emit_label` runs to revisit this — so remember
    // the withheld write (replacing any prior value for the same address) and arm the watchdog
    // to flush it if the grace elapses uncollected.
    if let Ok(mut p) = PENDING_EOP_LABELS.lock() {
        if let Some(slot) = p.iter_mut().find(|(a, _, _)| *a == addr) {
            slot.1 = value;
            slot.2 = width_bytes;
        } else {
            p.push((addr, value, width_bytes));
        }
    }
    arm_label_flush_watchdog();
}

/// Spawn (once) a thread that flushes a withheld EOP label if the title turns out to poll it.
///
/// This closes the gap the grace check alone cannot: the decision is made at submit time, but
/// a poller submits exactly once before it blocks on the label, so no later submit ever
/// re-runs the decision. The watchdog re-runs it from the outside. It wakes past the grace and
/// writes the remembered label iff the title still has not collected an equeue completion — an
/// equeue title trips that flag within milliseconds, so the watchdog finds nothing to do and
/// exits. It keeps checking briefly in case the deciding submit landed right at the boundary.
fn arm_label_flush_watchdog() {
    use std::sync::atomic::Ordering;
    if LABEL_WATCHDOG_ARMED.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::Builder::new()
        .name("unemups4-eop-label-watchdog".into())
        .spawn(|| {
            // A handful of checks spanning a little past the grace. Once a poller is confirmed
            // and flushed, `should_write_completion_label` stays true, so this can stop.
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if ps4_core::gpu::completion_event_registered() {
                    return; // equeue title: never flush.
                }
                if ps4_core::gpu::should_write_completion_label() {
                    // Drain and flush EVERY withheld label — a poller may wait on several — then
                    // stop. Take the entries out under the lock and write outside it (the write
                    // does not touch this static, but keeping the lock scope tight is cleaner).
                    let flush = PENDING_EOP_LABELS
                        .lock()
                        .map(|mut p| std::mem::take(&mut *p))
                        .unwrap_or_default();
                    if !flush.is_empty() {
                        for (addr, value, width_bytes) in flush {
                            unsafe { write_label(addr, value, width_bytes) };
                        }
                        return;
                    }
                }
            }
        })
        .ok();
}

/// Whether to restore the pre-task-157 synchronous inline EOP/EOS label write
/// (`UNEMUPS4_GPU_EOP_SYNC=1`). Default (unset / `0`) uses the pipelined-completion fix.
fn eop_sync_completion() -> bool {
    use std::sync::OnceLock;
    static SYNC: OnceLock<bool> = OnceLock::new();
    *SYNC.get_or_init(|| {
        std::env::var("UNEMUPS4_GPU_EOP_SYNC")
            .map(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Confirming experiment gate for task-174 (`UNEMUPS4_DUAL_CB_VS_ONLY`). When set, a
/// draw that declares BOTH a VS-CB and a PS-CB — which normally defers on the single
/// set0/bind2 const_storage slot — binds ONLY the VS-CB and drops the PS-CB instead of
/// deferring. Diagnostic-only: proves the defer is what starves the title's RT producers
/// (mountain/parallax) so the composite samples undefined memory. NOT the fix.
fn dual_cb_vs_only() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("UNEMUPS4_DUAL_CB_VS_ONLY")
            .map(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Parse `IT_EVENT_WRITE_EOP` and write its label. GFX6 body layout (5 dwords):
/// `[event_cntl, addr_lo, data_cntl(addr_hi in [15:0]), data_lo, data_hi]`. The
/// value is the full 64-bit `(data_hi:data_lo)` (DATA_SEL selects the width on real
/// HW; a 64-bit store covers the 32/64-bit label a CPU wait reads either way).
unsafe fn write_eop_label(body: &[u32]) {
    let [addr_lo, data_cntl, data_lo, data_hi] = match body {
        [_event_cntl, addr_lo, data_cntl, data_lo, data_hi, ..] => {
            [*addr_lo, *data_cntl, *data_lo, *data_hi]
        }
        _ => return,
    };
    let addr = label_addr(addr_lo, data_cntl);
    let value = (data_lo as u64) | ((data_hi as u64) << 32);
    // EOP fences can be 64-bit (DATA_SEL selects the width on real HW); a 64-bit store covers the
    // 32/64-bit label a CPU wait reads either way, so keep the full-width store here.
    emit_label(addr, value, 8);
}

/// Parse `IT_EVENT_WRITE_EOS` and write its label. GFX6 body layout (4 dwords):
/// `[event_cntl, addr_lo, cmd(addr_hi in [15:0]), data]`. EOS carries a single
/// 32-bit datum.
unsafe fn write_eos_label(body: &[u32]) {
    let [addr_lo, cmd, data] = match body {
        [_event_cntl, addr_lo, cmd, data, ..] => [*addr_lo, *cmd, *data],
        _ => return,
    };
    let addr = label_addr(addr_lo, cmd);
    // EOS carries a single 32-bit datum (GFX6 layout); store exactly 4 bytes so it neither
    // requires 8 bytes mapped to land nor zeroes a neighbouring 32-bit fence.
    emit_label(addr, data as u64, 4);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::ResourceCache;
    use crate::pm4::opcodes::t3_header;
    use crate::shader::embedded::EmbeddedShaderProvider;
    use crate::shader::source::{ChainProvider, ShaderRef, ShaderUnsupported, Stage};
    use crate::vbuf::{DataFormat, NumFormat, VertexAttribute};
    use ps4_core::gpu::PipelineId;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A `PresentSink` that counts `SubmitAndFlip`s and records every emitted
    /// `BackendCmd` list — the seam the draw ACs (#1/#3) exercise headlessly (no
    /// Vulkan). `run_command_list` is the phase-3.5 draw seam.
    #[derive(Default)]
    struct MockSink {
        flips: AtomicU32,
        /// `buf_idx` of the most recent `submit_and_flip`, so a test can assert the
        /// guest's scanout index reached the present sink (FIX 3 arg-decode guard).
        last_buf_idx: AtomicU32,
        cmds: Mutex<Vec<Vec<BackendCmd>>>,
    }
    impl PresentSink for MockSink {
        fn submit_and_flip(&self, _vo_handle: i32, buf_idx: u32) {
            self.flips.fetch_add(1, Ordering::SeqCst);
            self.last_buf_idx.store(buf_idx, Ordering::SeqCst);
        }
        fn run_command_list(&self, cmds: &[BackendCmd]) {
            self.cmds.lock().unwrap().push(cmds.to_vec());
        }
    }
    impl MockSink {
        fn command_lists(&self) -> Vec<Vec<BackendCmd>> {
            self.cmds.lock().unwrap().clone()
        }
    }

    /// Assert a single submit shipped ONE command list whose shape is exactly the
    /// embedded-draw miss path — `CreatePipeline(id) → BindPipeline(id) → SetViewport →
    /// SetScissor → DrawAuto(vertex_count)` — and return the minted id. An embedded VS
    /// reads `gl_VertexIndex`, so there is NO BindVertexBuffer. Independently reasoned:
    /// the expected variants/order (5 commands) are spelled out here, not derived from
    /// the production path. Also checks each stage's SPIR-V is a valid module (magic
    /// word) so a `CreatePipeline` cannot ship empty/garbage words unnoticed.
    fn assert_single_create_bind_draw(lists: &[Vec<BackendCmd>], vertex_count: u32) -> PipelineId {
        assert_eq!(lists.len(), 1, "exactly one command list per submit");
        let cmds = &lists[0];
        assert_eq!(
            cmds.len(),
            5,
            "an embedded first-use draw is Create + Bind + SetViewport + SetScissor + DrawAuto"
        );
        let created_id = match &cmds[0] {
            BackendCmd::CreatePipeline {
                id,
                vs_spirv,
                ps_spirv,
                ..
            } => {
                // 0x0723_0203 is the SPIR-V magic; a real module starts with it.
                assert_eq!(vs_spirv[0], 0x0723_0203, "VS SPIR-V must be a real module");
                assert_eq!(ps_spirv[0], 0x0723_0203, "PS SPIR-V must be a real module");
                *id
            }
            other => panic!("first command must be CreatePipeline, got {other:?}"),
        };
        match &cmds[1] {
            BackendCmd::BindPipeline { id } => {
                assert_eq!(
                    *id, created_id,
                    "BindPipeline must name the just-created id"
                )
            }
            other => panic!("second command must be BindPipeline, got {other:?}"),
        }
        assert!(
            matches!(&cmds[2], BackendCmd::SetViewport(_)),
            "third command must be SetViewport, got {:?}",
            cmds[2]
        );
        assert!(
            matches!(&cmds[3], BackendCmd::SetScissor(_)),
            "fourth command must be SetScissor, got {:?}",
            cmds[3]
        );
        match &cmds[4] {
            BackendCmd::DrawAuto { vertex_count: vc } => assert_eq!(*vc, vertex_count),
            other => panic!("fifth command must be DrawAuto, got {other:?}"),
        }
        created_id
    }

    /// Build a SubmitRange over an identity-mapped host buffer (host addr == guest
    /// ptr, doc-2 §1) so the executor decodes it in place.
    fn range_over(dcb: &[u32], flip: bool) -> SubmitRange {
        SubmitRange {
            dcb_ptr: dcb.as_ptr() as u64,
            dcb_size: (dcb.len() * 4) as u32,
            ccb_ptr: 0,
            ccb_size: 0,
            flip,
            vo_handle: 0,
            buf_idx: 0,
        }
    }

    /// A decoded vertex attribute for the layout tests, built directly from typed
    /// `dfmt`/`nfmt` (no V# encode/decode round-trip), so the expected vertex-input is
    /// reasoned from the format, not captured from the production mapping.
    fn vbuf_attr(stride: u32, dfmt: DataFormat, nfmt: NumFormat) -> VertexAttribute {
        VertexAttribute {
            stride,
            dfmt,
            nfmt,
            dst_sel: [4, 5, 6, 7], // identity swizzle; unused by the vertex-input mapping
        }
    }

    fn draw_buffers_with(attrs: Vec<VertexAttribute>) -> DrawBuffers {
        DrawBuffers {
            ranges: Vec::new(),
            deferred: Vec::new(),
            vertex_input: crate::vbuf::VertexInputDesc { attributes: attrs },
        }
    }

    #[test]
    fn vertex_layout_of_multi_attribute_non_vec4_hand_reasoned() {
        // AC #1/#2: a THREE-buffer draw whose attributes are NOT the tested single vec4:
        //   binding 0: position, _32_32_32 FLOAT, stride 12  → R32G32B32_SFLOAT
        //   binding 1: color,    _8_8_8_8   UNORM, stride 4   → R8G8B8A8_UNORM
        //   binding 2: uv,       _16_16     UNORM, stride 4   → R16G16_UNORM
        // The expected vertex-input is hand-reasoned from the format spec — the location
        // is the fetch order, the binding equals the location (one V# per attribute), the
        // offset is 0 (each attribute is base of its own buffer), and the stride is the
        // V# stride. NONE of these values are produced by the same helper that builds them.
        let db = draw_buffers_with(vec![
            vbuf_attr(12, DataFormat::Format32_32_32, NumFormat::Float),
            vbuf_attr(4, DataFormat::Format8_8_8_8, NumFormat::Unorm),
            vbuf_attr(4, DataFormat::Format16_16, NumFormat::Unorm),
        ]);

        let vl = match vertex_layout_of(&db) {
            Ok(Some(vl)) => vl,
            other => panic!("expected a built layout, got a defer/no-fetch: {other:?}"),
        };

        assert_eq!(vl.attribute_count, 3, "three attributes");
        assert_eq!(vl.binding_count, 3, "three bindings (one per buffer)");
        // Bindings: one per buffer, slot == fetch order, stride == the V# stride.
        assert_eq!(
            vl.bindings(),
            &[
                VertexBinding {
                    binding: 0,
                    stride: 12
                },
                VertexBinding {
                    binding: 1,
                    stride: 4
                },
                VertexBinding {
                    binding: 2,
                    stride: 4
                },
            ]
        );
        // Attributes: location/binding are the fetch order, offset 0, format is the
        // hand-reasoned dfmt/nfmt → VertexFormat (RGB32F, RGBA8 UNORM, RG16 UNORM).
        assert_eq!(
            vl.attributes(),
            &[
                VertexAttr {
                    location: 0,
                    binding: 0,
                    format: VertexFormat::R32G32B32Sfloat,
                    offset: 0
                },
                VertexAttr {
                    location: 1,
                    binding: 1,
                    format: VertexFormat::R8G8B8A8Unorm,
                    offset: 0
                },
                VertexAttr {
                    location: 2,
                    binding: 2,
                    format: VertexFormat::R16G16Unorm,
                    offset: 0
                },
            ]
        );
    }

    #[test]
    fn vertex_layout_of_unsupported_format_defers() {
        // AC #1 defer path: an attribute whose dfmt/nfmt maps to no host format
        // (Format8_8 SINT is not modeled) → Defer, NOT an empty layout. Distinct from the
        // embedded no-fetch case: a real fetching VS with a bad format must not silently
        // become an empty-vertex-input draw.
        let db = draw_buffers_with(vec![
            vbuf_attr(16, DataFormat::Format32_32_32_32, NumFormat::Float),
            vbuf_attr(2, DataFormat::Format8_8, NumFormat::Sint),
        ]);
        assert!(
            matches!(vertex_layout_of(&db), Err(())),
            "an unsupported attribute format must defer the draw"
        );
    }

    #[test]
    fn vertex_layout_of_no_fetch_is_empty_not_defer() {
        // AC #3: no vertex-buffer attributes (the embedded gl_VertexIndex path) → NoFetch
        // → the pipeline gets None (empty vertex input), unchanged.
        let db = draw_buffers_with(Vec::new());
        assert!(matches!(vertex_layout_of(&db), Ok(None)));
    }

    #[test]
    fn flip_submission_presents_through_sink() {
        // Tier A shape: a preamble packet, then the submission is a flip.
        let dcb = [t3_header(op::IT_NOP, 1), 0];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, true)) };
        assert_eq!(sink.flips.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn flip_buf_idx_reaches_sink() {
        // FIX 3 guard: the `buf_idx` a `SubmitAndFlip` carries must thread through the
        // executor to `PresentSink::submit_and_flip`, so a double-buffered title's second
        // scanout buffer (index 1) is the one presented.
        let dcb = [t3_header(op::IT_NOP, 1), 0];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        let mut range = range_over(&dcb, true);
        range.buf_idx = 1;
        unsafe { exec.run(&range) };
        assert_eq!(sink.flips.load(Ordering::SeqCst), 1);
        assert_eq!(sink.last_buf_idx.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn non_flip_submission_does_not_present() {
        let dcb = [t3_header(op::IT_NOP, 1), 0];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert_eq!(sink.flips.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn trace_only_mode_never_presents() {
        let dcb = [t3_header(op::IT_NOP, 1), 0];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::TraceOnly,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, true)) };
        assert_eq!(sink.flips.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn write_label_stores_64bit_value_to_mapped_guest_address() {
        // Unit-test the label STORE MECHANISM directly (task-157 split it from the emit
        // policy): the label lives in a host buffer whose address IS the guest GPU VA
        // (identity-mapped). `write_label` range-validates the untrusted address against the
        // bounded seam, then stores the 64-bit value. Wire a seam whose one mapped region
        // covers this label slot so the write lands.
        let mut label: u64 = 0;
        let label_addr = &mut label as *mut u64 as u64;
        let reader: std::sync::Arc<dyn ps4_core::bounded_read::BoundedRead> =
            std::sync::Arc::new(CountingRegionReader {
                start: label_addr,
                end: label_addr + 8,
                max_asked: AtomicU32::new(0),
            });
        let _seam = ps4_core::bounded_read::registered_source().override_scoped(reader);
        unsafe { write_label(label_addr, 0x0000_0001_CAFE_F00D, 8) };
        assert_eq!(label, 0x0000_0001_CAFE_F00D);
    }

    #[test]
    fn write_label_32bit_eos_leaves_adjacent_word_untouched() {
        // An EOS label is 32-bit (IT_EVENT_WRITE_EOS, GFX6). A 4-byte store must write only
        // [addr, addr+4) and leave the neighbouring 32-bit slot — which a title may use for an
        // ADJACENT fence — untouched. A full u64 store would zero it, resetting that neighbour
        // to "incomplete" and hanging a waiter on it.
        let mut slots: [u32; 2] = [0, 0xFEED_BEEF];
        let addr = slots.as_mut_ptr() as u64;
        let reader: std::sync::Arc<dyn ps4_core::bounded_read::BoundedRead> =
            std::sync::Arc::new(CountingRegionReader {
                start: addr,
                end: addr + 8,
                max_asked: AtomicU32::new(0),
            });
        let _seam = ps4_core::bounded_read::registered_source().override_scoped(reader);
        unsafe { write_label(addr, 0x0000_0000_CAFE_F00D, 4) };
        assert_eq!(slots[0], 0xCAFE_F00D, "the 32-bit label lands");
        assert_eq!(
            slots[1], 0xFEED_BEEF,
            "the adjacent 32-bit fence must not be zeroed by a too-wide store"
        );
    }

    #[test]
    fn eop_label_is_withheld_only_from_a_guest_that_collects_completions() {
        // Both arms of the completion-channel rule, in one test because the flag it reads is
        // a process-global.
        //
        // Withheld arm (task-157): our synchronous executor writing the label inline reports
        // GPU completion INSTANTLY, which makes the guest's gnmx recycle command buffers
        // without re-emitting per-frame texture binds (the Celeste white-bar bug). A guest
        // that COLLECTS completions from an equeue is told there instead, so the label stays
        // pending.
        //
        // Written arm: a guest that never waits on an equeue (the UE4 title) polls this word
        // in guest memory and has no other completion signal — withholding it wedges the
        // thread that would submit the next frame.
        let mut label: u64 = 0;
        let label_addr = &mut label as *mut u64 as u64;
        let addr_lo = (label_addr & 0xFFFF_FFFF) as u32;
        let addr_hi = ((label_addr >> 32) & 0xFFFF) as u32;

        let reader: std::sync::Arc<dyn ps4_core::bounded_read::BoundedRead> =
            std::sync::Arc::new(CountingRegionReader {
                start: label_addr,
                end: label_addr + 8,
                max_asked: AtomicU32::new(0),
            });
        let _seam = ps4_core::bounded_read::registered_source().override_scoped(reader);

        let dcb = [
            t3_header(op::IT_EVENT_WRITE_EOP, 5),
            0x0000_0004, // event_cntl (event type/index — unused here)
            addr_lo,
            addr_hi, // data_cntl: addr_hi in [15:0]
            0xCAFE_F00D,
            0x0000_0001, // data_hi
        ];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        // An equeue title (collected a completion): withhold on every frame, INCLUDING a
        // fresh boot where the grace has just elapsed. This is the case the screenshot broke.
        ps4_core::gpu::set_completion_event_registered(true);
        ps4_core::gpu::set_first_eop_submit_for_test(Some(
            std::time::Instant::now() - std::time::Duration::from_secs(10),
        ));
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert_eq!(
            label, 0,
            "an equeue-collecting guest must NOT get instant completion through the label"
        );

        // A label-poller during the boot grace: still withheld — silence is not yet proof.
        label = 0;
        ps4_core::gpu::set_completion_event_registered(false);
        ps4_core::gpu::set_first_eop_submit_for_test(Some(std::time::Instant::now()));
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert_eq!(
            label, 0,
            "within the boot grace, even an unwaited guest is withheld (could be an equeue title pre-first-wait)"
        );

        // Same poller past the grace: now written — it has proven it collects nothing.
        label = 0;
        ps4_core::gpu::set_first_eop_submit_for_test(Some(
            std::time::Instant::now() - std::time::Duration::from_secs(10),
        ));
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert_eq!(
            label, 0x0000_0001_CAFE_F00D,
            "past the grace, a guest that never collected a completion must be handed the label"
        );

        // Leave the process-globals as a fresh process has them.
        ps4_core::gpu::set_completion_event_registered(false);
        ps4_core::gpu::set_first_eop_submit_for_test(None);
    }

    #[test]
    fn zero_label_address_is_ignored() {
        // An EOP that only raises an interrupt names a null address; must no-op,
        // never fault.
        let dcb = [
            t3_header(op::IT_EVENT_WRITE_EOP, 5),
            0,
            0, // addr_lo = 0
            0,
            0xAAAA_AAAA,
            0xBBBB_BBBB,
        ];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
    }

    #[test]
    fn eop_label_to_unmapped_address_is_a_clean_noop() {
        // The EOP/EOS label address is register/packet-derived and untrusted. An address
        // that is NOT in a mapped guest range must NOT be written (an unbounded identity
        // store there would corrupt/over-write raw host memory — a SIGSEGV). `write_label`
        // range-validates through the bounded seam first, so a bad address is a clean no-op.
        //
        // Independent lever: `label` is a real, writable host slot, but the wired seam maps
        // a DIFFERENT region (not covering `label`). So the bounded check fails and the
        // write is skipped — `label` stays at its sentinel. Were the gate absent, the
        // identity store would land and the sentinel would change.
        let mut label: u64 = 0x0BAD_0BAD_0BAD_0BAD;
        let label_addr = &mut label as *mut u64 as u64;
        let addr_lo = (label_addr & 0xFFFF_FFFF) as u32;
        let addr_hi = ((label_addr >> 32) & 0xFFFF) as u32;

        // A mapped region that deliberately does NOT contain `label_addr` (placed one page
        // below it), so the seam reports the label slot unmapped.
        let mut other = [0u8; 16];
        let other_base = other.as_mut_ptr() as u64;
        let (region_start, region_end) = if other_base + 16 <= label_addr {
            (other_base, other_base + 16)
        } else {
            // `other` sits above the label; map a region strictly below the label instead.
            (label_addr.saturating_sub(0x2000), label_addr - 0x1000)
        };
        let reader: std::sync::Arc<dyn ps4_core::bounded_read::BoundedRead> =
            std::sync::Arc::new(CountingRegionReader {
                start: region_start,
                end: region_end,
                max_asked: AtomicU32::new(0),
            });
        let _seam = ps4_core::bounded_read::registered_source().override_scoped(reader);

        let dcb = [
            t3_header(op::IT_EVENT_WRITE_EOP, 5),
            0x0000_0004,
            addr_lo,
            addr_hi,
            0xCAFE_F00D,
            0x0000_0001,
        ];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert_eq!(
            label, 0x0BAD_0BAD_0BAD_0BAD,
            "an unmapped label address must not be written"
        );
    }

    #[test]
    fn truncated_eop_body_is_not_fatal() {
        // Header claims 5 body dwords but the buffer is short: decode yields a
        // Truncated packet (not Type3), so the arm is skipped — no panic.
        let dcb = [t3_header(op::IT_EVENT_WRITE_EOP, 5), 0x4, 0x1000];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
    }

    // ---- phase 3.5 embedded-shader draw arm ----

    /// A DrawIndexAuto (0x2D) packet body: [index_count, draw_initiator].
    fn draw_auto(index_count: u32) -> [u32; 3] {
        [t3_header(op::IT_DRAW_INDEX_AUTO, 2), index_count, 0]
    }

    #[test]
    fn embedded_bound_draw_dispatches_host_pipeline() {
        // AC #1: embedded VS id 0 + PS id 1 bound → DrawIndexAuto emits ONE list of
        // CreatePipeline + BindPipeline + DrawAuto onto the generic pipeline path
        // (recompiled/embedded SPIR-V crosses as Arc<[u32]>, NO GCN decode).
        let mut state = GpuState::default();
        state.bind_embedded_shader(Stage::Vertex, 0);
        state.bind_embedded_shader(Stage::Pixel, 1);

        let dcb = draw_auto(3);
        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        // First mint hands out id 1 (reasoned independently), and the shape is exactly
        // create → bind → draw.
        let id = assert_single_create_bind_draw(&sink.command_lists(), 3);
        assert_eq!(id, PipelineId(1));
    }

    /// A `DI_PT_RECTLIST` non-indexed draw is issued with FOUR vertices under a
    /// triangle-STRIP pipeline (task-184).
    ///
    /// GCN's rect list gives three vertices per RECTANGLE and synthesizes the fourth
    /// corner; issuing the same three under a triangle list covers half the target, which
    /// is why Celeste's bloom-target clears rasterized only a corner and the targets
    /// accumulated instead of being zeroed. Asserting the count AND the topology together
    /// matters: either alone still fails to cover the rectangle.
    #[test]
    fn rectlist_draw_is_issued_as_a_four_vertex_strip() {
        use crate::pm4::opcodes::{di_pt, reg_base, uconfig};

        let mut state = GpuState::default();
        state.bind_embedded_shader(Stage::Vertex, 0);
        state.bind_embedded_shader(Stage::Pixel, 1);

        let mut dcb = set_reg(
            op::IT_SET_UCONFIG_REG,
            uconfig::VGT_PRIMITIVE_TYPE - reg_base::UCONFIG,
            &[di_pt::RECTLIST],
        );
        dcb.extend_from_slice(&draw_auto(3));

        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        // The guest asked for 3; the rect needs its fourth corner.
        assert_single_create_bind_draw(&sink.command_lists(), 4);
        let lists = sink.command_lists();
        match &lists[0][0] {
            BackendCmd::CreatePipeline { key, .. } => assert_eq!(
                key.topology,
                ps4_core::gpu::PrimitiveTopology::TriangleStrip,
                "a rect list must build a triangle-STRIP pipeline; four vertices under a \
                 triangle LIST rasterize one triangle and cover half the rectangle"
            ),
            other => panic!("cmds[0] must be CreatePipeline, got {other:?}"),
        }
    }

    /// The counterpart: a triangle list is untouched. Modelling `VGT_PRIMITIVE_TYPE` must
    /// not change the vertex count or the topology of every draw that came before it.
    #[test]
    fn trilist_draw_keeps_its_vertex_count_and_topology() {
        use crate::pm4::opcodes::{di_pt, reg_base, uconfig};

        let mut state = GpuState::default();
        state.bind_embedded_shader(Stage::Vertex, 0);
        state.bind_embedded_shader(Stage::Pixel, 1);

        let mut dcb = set_reg(
            op::IT_SET_UCONFIG_REG,
            uconfig::VGT_PRIMITIVE_TYPE - reg_base::UCONFIG,
            &[di_pt::TRILIST],
        );
        dcb.extend_from_slice(&draw_auto(3));

        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        assert_single_create_bind_draw(&sink.command_lists(), 3);
        let lists = sink.command_lists();
        match &lists[0][0] {
            BackendCmd::CreatePipeline { key, .. } => {
                assert_eq!(key.topology, ps4_core::gpu::PrimitiveTopology::TriangleList)
            }
            other => panic!("cmds[0] must be CreatePipeline, got {other:?}"),
        }
    }

    #[test]
    fn non_embedded_bound_draw_defers_not_fatal() {
        // AC #3: a draw bound to a real .sb GCN shader is cleanly detected and
        // deferred — NO BackendCmd emitted, no crash.
        let mut state = GpuState::default();
        state.shaders.set(
            Stage::Vertex,
            crate::shader::source::ShaderRef::GcnBinary {
                addr: 0xE000,
                res: crate::shader::source::GcnResources::default(),
                ps_input_map: ps4_gcn::PsInputMap::default(),
            },
        );
        state.bind_embedded_shader(Stage::Pixel, 1);

        let dcb = draw_auto(3);
        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        assert!(
            sink.command_lists().is_empty(),
            "GCN bind must defer, not draw"
        );
    }

    #[test]
    fn pgm_reg_bound_draw_defers_as_needs_gcn() {
        // AC #1: a DCB that programs SPI_SHADER_PGM_LO/HI_VS + _PS via
        // SET_SH_REG then issues DRAW_INDEX_AUTO resolves to NeedsGcn (a register-
        // derived .sb GCN bind), so the draw defers cleanly — NO BackendCmd emitted.
        use crate::pm4::opcodes::sh_reg;
        use crate::shader::sb::pgm_addr;
        use crate::shader::source::ShaderRef;

        let sh = |abs: u32| abs - reg_base_sh();
        // VS code at 0x0020_0000 → >>8 split across LO/HI.
        let mut dcb = set_reg(
            op::IT_SET_SH_REG,
            sh(sh_reg::SPI_SHADER_PGM_LO_VS),
            &[0x0000_2000, 0x0000_0000],
        );
        // PS code at 0x0030_0000.
        dcb.extend(set_reg(
            op::IT_SET_SH_REG,
            sh(sh_reg::SPI_SHADER_PGM_LO_PS),
            &[0x0000_3000, 0x0000_0000],
        ));
        dcb.extend_from_slice(&draw_auto(3));

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        // The GCN bind defers (AC #3-style clean skip), no host pipeline dispatched.
        assert!(
            sink.command_lists().is_empty(),
            "a register-programmed GCN shader must defer, not draw"
        );
        // The derived view carries the correct .sb code addr for both stages.
        let bound = state.derive_bound_shaders();
        assert!(matches!(
            bound.vs,
            Some(ShaderRef::GcnBinary { addr, .. }) if addr == pgm_addr(0x2000, 0)
        ));
        assert!(matches!(
            bound.ps,
            Some(ShaderRef::GcnBinary { addr, .. }) if addr == pgm_addr(0x3000, 0)
        ));
    }

    /// SH register-window base, for turning an absolute SH index into a SET_SH_REG
    /// per-packet offset in the tests above.
    fn reg_base_sh() -> u32 {
        crate::pm4::opcodes::reg_base::SH
    }

    #[test]
    fn unbound_draw_skips_cleanly() {
        // No shaders bound → the draw is skipped, no BackendCmd, no crash.
        let mut state = GpuState::default();
        let dcb = draw_auto(3);
        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert!(sink.command_lists().is_empty());
    }

    #[test]
    fn embedded_draw_with_registered_target_still_dispatches() {
        // A draw that programs a supported color target aliasing a registered display
        // buffer derives cleanly and dispatches the embedded pipeline (AC #1 at the
        // executor seam).
        use crate::pm4::opcodes::context_reg as ctx;
        use ps4_core::gpu::{DisplayBuffer, DisplayBufferSource, registered_display_buffers};
        use std::sync::Arc;

        struct OneBuffer(DisplayBuffer);
        impl DisplayBufferSource for OneBuffer {
            fn lookup(&self, base: u64) -> Option<DisplayBuffer> {
                (self.0.base == base).then_some(self.0)
            }
        }
        let fb_base = 0xC000_0000u64;
        let src: Arc<dyn DisplayBufferSource> = Arc::new(OneBuffer(DisplayBuffer {
            base: fb_base,
            width: 1920,
            height: 1080,
        }));
        let _guard = registered_display_buffers().override_scoped(src);

        let mut state = GpuState::default();
        state.bind_embedded_shader(Stage::Vertex, 0);
        state.bind_embedded_shader(Stage::Pixel, 1);
        state
            .ctx_regs
            .set(ctx::CB_COLOR0_BASE, (fb_base >> 8) as u32);
        state.ctx_regs.set(ctx::CB_COLOR0_INFO, 0x0A << 2);

        let dcb = draw_auto(3);
        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        // Dispatches through the generic path: create → bind → draw, one list.
        assert_single_create_bind_draw(&sink.command_lists(), 3);
    }

    #[test]
    fn embedded_draw_with_unsupported_target_format_defers() {
        // AC #3 at the executor seam: an embedded-bound draw that programs an
        // unsupported CB_COLOR0_INFO format defers cleanly — NO BackendCmd, no crash.
        use crate::pm4::opcodes::context_reg as ctx;
        use ps4_core::gpu::{DisplayBuffer, DisplayBufferSource, registered_display_buffers};
        use std::sync::Arc;

        struct OneBuffer(DisplayBuffer);
        impl DisplayBufferSource for OneBuffer {
            fn lookup(&self, base: u64) -> Option<DisplayBuffer> {
                (self.0.base == base).then_some(self.0)
            }
        }
        let fb_base = 0xC000_0000u64;
        let src: Arc<dyn DisplayBufferSource> = Arc::new(OneBuffer(DisplayBuffer {
            base: fb_base,
            width: 1920,
            height: 1080,
        }));
        let _guard = registered_display_buffers().override_scoped(src);

        let mut state = GpuState::default();
        state.bind_embedded_shader(Stage::Vertex, 0);
        state.bind_embedded_shader(Stage::Pixel, 1);
        state
            .ctx_regs
            .set(ctx::CB_COLOR0_BASE, (fb_base >> 8) as u32);
        // An unmapped FORMAT value → derive_target returns UnsupportedFormat.
        state.ctx_regs.set(ctx::CB_COLOR0_INFO, 0x1 << 2);

        let dcb = draw_auto(3);
        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        assert!(
            sink.command_lists().is_empty(),
            "unsupported RT format must defer, not draw"
        );
    }

    #[test]
    fn present_subset_mode_ignores_draw_arm() {
        // The draw arm is gated on ExecMode::Draw: in PresentSubset a DrawIndexAuto
        // is decoded but not dispatched (phase-3 present-only stays present-only).
        let mut state = GpuState::default();
        state.bind_embedded_shader(Stage::Vertex, 0);
        state.bind_embedded_shader(Stage::Pixel, 1);

        let dcb = draw_auto(3);
        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert!(sink.command_lists().is_empty());
    }

    #[test]
    fn draw_mode_still_presents_on_flip() {
        // Draw mode is a superset of PresentSubset: a flip still presents.
        let dcb = [t3_header(op::IT_NOP, 1), 0];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, true)) };
        assert_eq!(sink.flips.load(Ordering::SeqCst), 1);
    }

    // ---- §C7 shadow register file: SET_*_REG / IT_CLEAR_STATE apply during run ----

    /// Build a SET_*_REG packet: header + [reg_offset, values...].
    fn set_reg(opcode: u8, reg_offset: u32, values: &[u32]) -> Vec<u32> {
        let body_len = 1 + values.len();
        let mut pkt = vec![t3_header(opcode, body_len), reg_offset];
        pkt.extend_from_slice(values);
        pkt
    }

    #[test]
    fn set_context_reg_applies_into_state_during_run() {
        // AC #1: a decoded SET_CONTEXT_REG multi-dword packet lands values at the
        // right absolute indices (base + offset + i) in the CONTEXT bank.
        use crate::pm4::opcodes::reg_base;
        let dcb = set_reg(op::IT_SET_CONTEXT_REG, 0x20, &[0x111, 0x222, 0x333]);
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert_eq!(state.ctx_regs.get(reg_base::CONTEXT + 0x20), Some(0x111));
        assert_eq!(state.ctx_regs.get(reg_base::CONTEXT + 0x21), Some(0x222));
        assert_eq!(state.ctx_regs.get(reg_base::CONTEXT + 0x22), Some(0x333));
    }

    #[test]
    fn set_reg_applies_in_draw_mode_too() {
        // The apply is on every present_sync mode (PresentSubset up), incl. Draw.
        use crate::pm4::opcodes::reg_base;
        let dcb = set_reg(op::IT_SET_SH_REG, 0x4, &[0xABCD]);
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert_eq!(state.sh_regs.get(reg_base::SH + 0x4), Some(0xABCD));
    }

    #[test]
    fn trace_only_mode_does_not_apply_set_reg() {
        // TraceOnly returns before the packet loop: no state mutation.
        let dcb = set_reg(op::IT_SET_CONTEXT_REG, 0, &[0x1]);
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::TraceOnly,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert!(state.ctx_regs.is_empty());
    }

    #[test]
    fn clear_state_resets_banks_within_run() {
        // AC #2: IT_CLEAR_STATE in the same submission resets banks written before it.
        let mut dcb = set_reg(op::IT_SET_CONTEXT_REG, 0, &[0x1, 0x2]);
        dcb.push(t3_header(op::IT_CLEAR_STATE, 1));
        dcb.push(0);
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::PresentSubset,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert!(state.ctx_regs.is_empty());
    }

    #[test]
    fn state_persists_across_submits_until_cleared() {
        // AC #2: register state set in submit N is visible in submit N+1 (the state
        // is borrowed across executor instances), until IT_CLEAR_STATE resets it.
        use crate::pm4::opcodes::reg_base;
        let sink = MockSink::default();
        let mut state = GpuState::default();

        let dcb1 = set_reg(op::IT_SET_UCONFIG_REG, 0x8, &[0x55]);
        {
            let embedded = EmbeddedShaderProvider::new();
            let providers: [&dyn ShaderProvider; 1] = [&embedded];
            let chain = ChainProvider::new(&providers);
            let mut pipelines = PipelineCache::new();
            let mut resources = ResourceCache::new();
            let mut exec = Executor::new(
                ExecMode::PresentSubset,
                &sink,
                &mut state,
                &chain,
                &mut pipelines,
                &mut resources,
            );
            unsafe { exec.run(&range_over(&dcb1, false)) };
        }
        assert_eq!(state.uconfig_regs.get(reg_base::UCONFIG + 0x8), Some(0x55));

        // A second, unrelated submission does not clear the prior write.
        let dcb2 = [t3_header(op::IT_NOP, 1), 0];
        {
            let embedded = EmbeddedShaderProvider::new();
            let providers: [&dyn ShaderProvider; 1] = [&embedded];
            let chain = ChainProvider::new(&providers);
            let mut pipelines = PipelineCache::new();
            let mut resources = ResourceCache::new();
            let mut exec = Executor::new(
                ExecMode::PresentSubset,
                &sink,
                &mut state,
                &chain,
                &mut pipelines,
                &mut resources,
            );
            unsafe { exec.run(&range_over(&dcb2, false)) };
        }
        assert_eq!(state.uconfig_regs.get(reg_base::UCONFIG + 0x8), Some(0x55));

        // A third submission clears it.
        let dcb3 = [t3_header(op::IT_CLEAR_STATE, 1), 0];
        {
            let embedded = EmbeddedShaderProvider::new();
            let providers: [&dyn ShaderProvider; 1] = [&embedded];
            let chain = ChainProvider::new(&providers);
            let mut pipelines = PipelineCache::new();
            let mut resources = ResourceCache::new();
            let mut exec = Executor::new(
                ExecMode::PresentSubset,
                &sink,
                &mut state,
                &chain,
                &mut pipelines,
                &mut resources,
            );
            unsafe { exec.run(&range_over(&dcb3, false)) };
        }
        assert!(state.uconfig_regs.is_empty());
    }

    /// A bounded reader over a single `[start, end)` host region that also caps the largest
    /// single read it will satisfy: any `read_ranged` whose range crosses `end` (the mapping
    /// boundary) is a clean `Err`, never an over-read. It records the largest span it was
    /// *asked* to read so the test can prove the seam was consulted for a bounded read.
    struct CountingRegionReader {
        start: u64,
        end: u64,
        max_asked: AtomicU32,
    }
    impl ps4_core::bounded_read::BoundedRead for CountingRegionReader {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            self.max_asked.fetch_max(size as u32, Ordering::SeqCst);
            if size == 0 {
                return Ok(Vec::new());
            }
            let range_end = addr.checked_add(size as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end {
                return Err("start not mapped");
            }
            if range_end > self.end {
                return Err("range crosses region boundary");
            }
            let mut buf = vec![0u8; size];
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size);
            }
            Ok(buf)
        }
    }

    #[test]
    fn parse_sb_bounded_over_guest_pgm_addr_cannot_over_read() {
        use crate::shader::sb::pgm_addr;
        use ps4_core::bounded_read::{BoundedRead, registered_source};
        use std::sync::Arc;

        // A tiny host arena standing in for the guest's mapped shader region. Fill it with
        // garbage containing NO OrbShdr magic, and map only the first half as the VMA. A
        // register-derived (untrusted) shader address placed near the mapped end would, over
        // an UNBOUNDED reader, let parse_sb's magic scan walk up to 1 MiB past `region_end`
        // into raw host memory (SIGSEGV / adjacent-memory leak). Through the bounded seam the
        // scan must stop at the boundary and reject cleanly.
        let arena = vec![0xABu8; 4096];
        let base = arena.as_ptr() as u64;
        let region_end = base + 2048; // only [base, base+2048) is "mapped"

        let reader = Arc::new(CountingRegionReader {
            start: base,
            end: region_end,
            max_asked: AtomicU32::new(0),
        });

        // A guest-controlled PGM_LO/HI pair naming a code_start a few bytes shy of the mapped
        // end: the first scan window already runs past `region_end`.
        let code_start = region_end - 8;
        let shifted = code_start >> 8;
        let pgm_lo = (shifted & 0xFFFF_FFFF) as u32;
        let pgm_hi = (shifted >> 32) as u32;
        let derived = pgm_addr(pgm_lo, pgm_hi);

        // RAII override: the guard serializes against other bounded-read tests and restores
        // the prior source on drop — even on panic — so no wired reader leaks into later
        // ps4-gnm tests (replaces the raw register/clear + module SEAM_LOCK).
        {
            let src: Arc<dyn BoundedRead> = reader.clone();
            let _guard = registered_source().override_scoped(src);

            // parse_sb_bounded reads through the derived address; a bogus/near-boundary address
            // must reject cleanly (no OrbShdr magic reachable inside the mapping), never crash.
            let err = parse_sb_bounded(derived).unwrap_err();
            assert!(
                matches!(err, SbParseError::MagicNotFound | SbParseError::MemoryFault),
                "near-boundary guest PGM addr must reject cleanly, got {err:?}"
            );
            // The seam WAS consulted (the parser issued at least one read through it) and NEVER
            // asked for a read that reached past the mapping — i.e. no unbounded 1 MiB request
            // slipped through. The largest span asked stays within the ~4 KiB scan chunk, far
            // below MAX_SCAN_BYTES; and every read crossing `region_end` was rejected by the
            // reader, so the derived address could not over-read.
            let asked = reader.max_asked.load(Ordering::SeqCst);
            assert!(
                asked > 0,
                "parse_sb_bounded never consulted the bounded seam"
            );
            assert!(
                asked < (1 << 20),
                "seam was asked for a {asked}-byte read — an unbounded scan slipped through"
            );
        }

        // Headless degradation: with no source wired, parse refuses rather than falling back
        // to an unbounded identity read of the untrusted pointer. The `_none` guard forces the
        // global unregistered for this scope and restores it on drop.
        {
            let _none = registered_source().override_none_scoped();
            assert_eq!(
                parse_sb_bounded(derived),
                Err(SbParseError::MemoryFault),
                "headless parse_sb_bounded must reject, not read unbounded"
            );
        }
    }

    /// A `ShaderProvider` that counts every `resolve` call per `ShaderRef`, so a test can
    /// assert the executor consults the (potentially expensive/side-effecting) provider
    /// EXACTLY ONCE per stage — no double-resolve per draw.
    #[derive(Default)]
    struct CountingProvider {
        vs_calls: AtomicU32,
        ps_calls: AtomicU32,
    }
    impl ShaderProvider for CountingProvider {
        fn resolve(
            &self,
            r: &ShaderRef,
            mem: &dyn ps4_core::memory::VirtualMemoryManager,
            dirty: Option<&dyn ps4_core::dirty::DirtySource>,
        ) -> Result<Option<crate::shader::source::HostShader>, ShaderUnsupported> {
            match r {
                ShaderRef::Embedded {
                    stage: Stage::Vertex,
                    ..
                } => {
                    self.vs_calls.fetch_add(1, Ordering::SeqCst);
                }
                ShaderRef::Embedded {
                    stage: Stage::Pixel,
                    ..
                } => {
                    self.ps_calls.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            // Defer resolution to the real embedded provider so the draw outcome is unchanged.
            EmbeddedShaderProvider::new().resolve(r, mem, dirty)
        }
    }

    #[test]
    fn each_shader_ref_resolved_exactly_once_per_draw() {
        // AC #1: a draw resolving both bound stages consults the provider exactly once per
        // stage (each stage is resolved once so a side-effecting provider's parse+recompile
        // is not run twice per draw).
        let mut state = GpuState::default();
        state.bind_embedded_shader(Stage::Vertex, 0);
        state.bind_embedded_shader(Stage::Pixel, 1);

        let dcb = draw_auto(3);
        let sink = MockSink::default();
        let counting = CountingProvider::default();
        let providers: [&dyn ShaderProvider; 1] = [&counting];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        assert_eq!(
            counting.vs_calls.load(Ordering::SeqCst),
            1,
            "VS ref must be resolved exactly once per draw"
        );
        assert_eq!(
            counting.ps_calls.load(Ordering::SeqCst),
            1,
            "PS ref must be resolved exactly once per draw"
        );
        // And the outcome is unchanged: the pair still dispatches a host pipeline.
        assert_single_create_bind_draw(&sink.command_lists(), 3);
    }

    #[test]
    fn rebound_pipeline_across_submits_reuses_id_no_second_create() {
        // AC #1 (cache keyed correctly): the SAME embedded VS/PS pair drawn in two
        // separate submits mints ONE pipeline id and emits CreatePipeline only the
        // FIRST time — the second submit ships only BindPipeline + DrawAuto. The cache
        // persists across submits (it lives in the driver), so the executor here shares
        // one PipelineCache across both runs.
        let mut state = GpuState::default();
        state.bind_embedded_shader(Stage::Vertex, 0);
        state.bind_embedded_shader(Stage::Pixel, 1);

        let dcb = draw_auto(3);
        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();

        // Submit 1: cache miss → CreatePipeline(id 1) + BindPipeline + DrawAuto.
        {
            let mut exec = Executor::new(
                ExecMode::Draw,
                &sink,
                &mut state,
                &chain,
                &mut pipelines,
                &mut resources,
            );
            unsafe { exec.run(&range_over(&dcb, false)) };
        }
        // Submit 2: same key → HIT, so no second CreatePipeline.
        {
            let mut exec = Executor::new(
                ExecMode::Draw,
                &sink,
                &mut state,
                &chain,
                &mut pipelines,
                &mut resources,
            );
            unsafe { exec.run(&range_over(&dcb, false)) };
        }

        let lists = sink.command_lists();
        assert_eq!(lists.len(), 2, "two submits → two lists");
        // Reasoned-independent expected shapes: submit 1 is the 3-command miss path;
        // submit 2 is the 2-command hit path binding the SAME id.
        let created = assert_single_create_bind_draw(&lists[..1], 3);
        assert_eq!(created, PipelineId(1));
        // Submit 2 is the hit path: BindPipeline (same id) + viewport/scissor + DrawAuto,
        // NO second CreatePipeline. Reasoned-independent: 4 commands, id 1.
        assert_eq!(
            lists[1].len(),
            4,
            "hit path is bind + viewport + scissor + draw"
        );
        assert_eq!(lists[1][0], BackendCmd::BindPipeline { id: PipelineId(1) });
        assert!(matches!(lists[1][1], BackendCmd::SetViewport(_)));
        assert!(matches!(lists[1][2], BackendCmd::SetScissor(_)));
        assert_eq!(lists[1][3], BackendCmd::DrawAuto { vertex_count: 3 });
        // The guest-side cache minted exactly ONE pipeline (asserted against the
        // reasoned value 1, not a value the production path re-derived).
        assert_eq!(pipelines.created_count(), 1);
    }

    // ---- AC #1 / #2: real-GCN-shader end-to-end over a corpus .sb blob ----

    use crate::pm4::opcodes::sh_reg;
    use ps4_core::bounded_read::{BoundedRead, registered_source};
    use ps4_core::gpu::{ColorFormat, IndexType, ResourceId};
    use std::path::Path;
    use std::sync::Arc;

    /// A committed corpus `.sb` blob (built + header-checked by the `ps4-gcn` crate).
    fn corpus_sb(name: &str) -> Vec<u8> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gcn/tests/corpus")
            .join(format!("{name}.sb"));
        std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    /// A flat backing-buffer bounded reader: guest addr == the real host address of
    /// `buf[addr - raw]`. Bounds-checked (an over-read is a clean fault). `raw` is the
    /// Vec's own host base, so guest == host addressing holds and a 256-aligned guest
    /// address (needed for a `.sb` PGM address) round-trips. The V#/index/`.sb` reads the
    /// executor issues all route through this seam (never a bare identity view).
    struct ArenaReader {
        raw: u64,
        buf: Vec<u8>,
    }
    impl BoundedRead for ArenaReader {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            let start = addr.checked_sub(self.raw).ok_or("below arena")? as usize;
            let end = start.checked_add(size).ok_or("overflow")?;
            if end > self.buf.len() {
                return Err("past arena");
            }
            Ok(self.buf[start..end].to_vec())
        }
    }

    /// Build the four little-endian dwords of a vec4-float vertex-buffer V# (dfmt 14 =
    /// `_32_32_32_32`, nfmt 7 = float, identity swizzle) — the descriptor the corpus VS
    /// fetches. Hand-laid so the test's expectations do not come from `decode_v_sharp`.
    fn vec4_vsharp(base: u64, stride: u32, num_records: u32) -> [u8; 16] {
        let w0 = (base & 0xFFFF_FFFF) as u32;
        let w1 = ((base >> 32) as u32 & 0xFFFF) | ((stride & 0x3FFF) << 16);
        let w2 = num_records;
        // dst_sel x=4 y=5 z=6 w=7, nfmt=7 (float) [14:12], dfmt=14 [18:15].
        let w3: u32 = 4 | (5 << 3) | (6 << 6) | (7 << 9) | (7 << 12) | (14 << 15);
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&w0.to_le_bytes());
        out[4..8].copy_from_slice(&w1.to_le_bytes());
        out[8..12].copy_from_slice(&w2.to_le_bytes());
        out[12..16].copy_from_slice(&w3.to_le_bytes());
        out
    }

    /// Program the VS+PS `SPI_SHADER_PGM_LO/HI` binds for a `.sb` GCN shader at 256-aligned
    /// guest addresses via SET_SH_REG (the register-bind path `derive_bound_shaders` reads).
    fn bind_gcn_shaders(dcb: &mut Vec<u32>, vs_addr: u64, ps_addr: u64) {
        let sh = |abs: u32| abs - reg_base_sh();
        let vs = vs_addr >> 8;
        let ps = ps_addr >> 8;
        dcb.extend(set_reg(
            op::IT_SET_SH_REG,
            sh(sh_reg::SPI_SHADER_PGM_LO_VS),
            &[(vs & 0xFFFF_FFFF) as u32, (vs >> 32) as u32],
        ));
        dcb.extend(set_reg(
            op::IT_SET_SH_REG,
            sh(sh_reg::SPI_SHADER_PGM_LO_PS),
            &[(ps & 0xFFFF_FFFF) as u32, (ps >> 32) as u32],
        ));
    }

    /// Program the VS user-SGPR pair `s[2:3]` to the descriptor-set pointer (corpus ABI).
    fn bind_vs_desc_set(dcb: &mut Vec<u32>, desc_ptr: u64) {
        let sh = |abs: u32| abs - reg_base_sh();
        dcb.extend(set_reg(
            op::IT_SET_SH_REG,
            sh(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 2),
            &[(desc_ptr & 0xFFFF_FFFF) as u32, (desc_ptr >> 32) as u32],
        ));
    }

    /// Copy `bytes` into `arena` at the next offset whose GUEST ADDRESS (`base + offset`)
    /// is 256-aligned at/after `cursor`, returning the guest address it landed at and the
    /// new cursor. 256-alignment of the guest address is required for a `.sb` PGM address
    /// (`SET_SH_REG` writes `addr >> 8`, which must round-trip to the same address).
    fn place_aligned(arena: &mut Vec<u8>, base: u64, cursor: usize, bytes: &[u8]) -> (u64, usize) {
        let mut start = cursor;
        while (base + start as u64) & 0xFF != 0 {
            start += 1;
        }
        if arena.len() < start {
            arena.resize(start, 0);
        }
        let addr = base + start as u64;
        arena.extend_from_slice(bytes);
        (addr, arena.len())
    }

    #[test]
    fn gcn_draw_auto_end_to_end_emits_create_storagebind_draw() {
        // AC #1: a synthetic DCB that binds a real corpus VS (passthrough_vs, which fetches
        // a vec4 vertex buffer through an SSBO indexed by gl_VertexIndex) + PS
        // (flat_color_ps) via SET_SH_REG, programs the VS descriptor-set pointer (s[2:3]) at
        // a hand-laid vec4 V#, and issues DRAW_INDEX_AUTO → the executor recompiles the .sb
        // through the driver-owned chain and emits the EXACT create/storage-bind/
        // viewport/scissor/draw sequence. The recompiled passthrough_vs consumes no
        // vertex-input: it fetches from an SSBO clamped by a num_records push constant, so
        // the pipeline key carries no vertex layout and the vertex data is bound as a
        // storage buffer. Every expected value (ids, ordering, count, num_records) is an
        // independently-reasoned literal, NOT a value the production path produced.

        // Arena holding the two .sb blobs (256-aligned), the descriptor set (one vec4 V#),
        // and the vertex data. Its host base IS the guest base (bounded-seam addressing).
        let vs_blob = corpus_sb("passthrough_vs");
        let ps_blob = corpus_sb("flat_color_ps");
        // Reserve enough capacity so the Vec never reallocates while we lay it out —
        // its host base is the guest base and must stay stable.
        let mut arena: Vec<u8> = Vec::with_capacity(0x2000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (ps_addr, cur) = place_aligned(&mut arena, base, cur, &ps_blob);
        // Vertex data: 3 vec4-float vertices (48 bytes), placed 16-aligned.
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        // Descriptor set = one vec4 V# at binding 0 → base=vtx_addr, stride 16, 3 records.
        let desc_off = arena.len();
        let desc_ptr = base + desc_off as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // Build the DCB: SH-reg shader binds + VS descriptor-set pointer + DRAW_INDEX_AUTO.
        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, ps_addr);
        bind_vs_desc_set(&mut dcb, desc_ptr);
        dcb.extend_from_slice(&draw_auto(3));

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        // Expected list (independently reasoned): a real-VS first-use draw whose VS fetches
        // through an SSBO is exactly:
        //   [0] CreatePipeline{id 1, recompiled VS+PS SPIR-V, vertex_layout None,
        //       storage Some(set 0/binding 0/stride 16), push_constants Some(0..4)}
        //   [1] BindPipeline{id 1}
        //   [2] CreateBuffer{res id 1, 48}        (vertex data, first use)
        //   [3] UploadBuffer{res id 1, 48 bytes}
        //   [4] BindStorageBuffer{set 0, binding 0, res id 1, num_records 3}
        //   [5] SetViewport, [6] SetScissor, [7] DrawAuto{3}
        let lists = sink.command_lists();
        assert_eq!(lists.len(), 1, "one submit → one command list");
        let cmds = &lists[0];
        assert_eq!(
            cmds.len(),
            8,
            "real-VS draw: create + bind + createbuf + upload + storagebind + vp + sc + draw"
        );
        match &cmds[0] {
            BackendCmd::CreatePipeline {
                id,
                vs_spirv,
                ps_spirv,
                key,
                vertex_storage,
                push_constants,
                ..
            } => {
                assert_eq!(*id, PipelineId(1), "first pipeline id is 1");
                assert_eq!(vs_spirv[0], 0x0723_0203, "VS SPIR-V is a real module");
                assert_eq!(ps_spirv[0], 0x0723_0203, "PS SPIR-V is a real module");
                // The SSBO-fetch VS consumes no vertex-input, so the key carries no layout.
                assert!(
                    key.vertex_layout.is_none(),
                    "an SSBO-fetch VS keys no vertex layout"
                );
                // One vec4 V# → one storage binding (set 0, binding 0, stride 16) and the
                // num_records+stride+dst_sel+format push-constant range (offset 0, size 16 =
                // four uints). Hand-reasoned (task-140, task-155, task-164).
                assert_eq!(
                    *vertex_storage,
                    vec![StorageBinding {
                        set: 0,
                        binding: 0,
                        stride: 16
                    }],
                    "vec4 SSBO binding: one stream at set 0, binding 0, 16-byte stride"
                );
                assert_eq!(
                    *push_constants,
                    Some(PushConstantRange {
                        offset: 0,
                        size: 16
                    }),
                    "num_records+stride+dst_sel+format push constant: offset 0, size 16"
                );
            }
            other => panic!("cmds[0] must be CreatePipeline, got {other:?}"),
        }
        assert_eq!(cmds[1], BackendCmd::BindPipeline { id: PipelineId(1) });
        assert_eq!(
            cmds[2],
            BackendCmd::CreateBuffer {
                id: ResourceId(1),
                size: 48
            },
            "first vertex-data use creates res id 1 of 48 bytes"
        );
        match &cmds[3] {
            BackendCmd::UploadBuffer { id, offset, data } => {
                assert_eq!(*id, ResourceId(1));
                assert_eq!(*offset, 0);
                assert_eq!(data.len(), 48, "the 48-byte vertex buffer is uploaded");
            }
            other => panic!("cmds[3] must be UploadBuffer, got {other:?}"),
        }
        assert_eq!(
            cmds[4],
            BackendCmd::BindStorageBuffer {
                set: 0,
                binding: 0,
                id: ResourceId(1),
                num_records: 3,
                stride: 16,
                // vec4_vsharp encodes the identity swizzle [4,5,6,7] → word3[11:0] = 0xFAC.
                dst_sel: 0xFAC,
                // vec4_vsharp is dfmt 14 (_32_32_32_32) / nfmt 7 (float) → packed 0x70E; the
                // recompiled VS takes the raw-dword path for it (task-164).
                format: 0x70E,
                // Single stream → its push-constant group is at offset 0.
                pc_offset: 0,
            }
        );
        assert!(matches!(cmds[5], BackendCmd::SetViewport(_)));
        assert!(matches!(cmds[6], BackendCmd::SetScissor(_)));
        assert_eq!(cmds[7], BackendCmd::DrawAuto { vertex_count: 3 });
    }

    /// Program the VS user-SGPR quad `s[4:7]` with an inline 128-bit V# (the constant-buffer
    /// SBASE convention `cbuffer16_vs` / retail Celeste VS use — the V# sits IN the SGPRs, not
    /// behind a descriptor-set pointer). `words` are the four V# dwords.
    fn bind_vs_const_vsharp(dcb: &mut Vec<u32>, words: &[u8; 16]) {
        let sh = |abs: u32| abs - reg_base_sh();
        let w = |i: usize| u32::from_le_bytes([words[i], words[i + 1], words[i + 2], words[i + 3]]);
        dcb.extend(set_reg(
            op::IT_SET_SH_REG,
            sh(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 4),
            &[w(0), w(4), w(8), w(12)],
        ));
    }

    #[test]
    fn gcn_const_buffer_vs_binds_const_ssbo() {
        // Gap 2 (task-113.4.1): a VS that reads scalar constants via s_buffer_load
        // (cbuffer16_vs — the 4×4 transform-matrix load the retail Celeste VS emit, doc-6
        // Entry 9) declares a const_buffers SSBO at set0/bind2. The executor must resolve its
        // V# from the VS user-SGPR block (s[4:7], inline), build the pipeline with a
        // `const_storage` descriptor, and emit a `BindConstBuffer` pointing at the CB bytes —
        // never a pipeline with an unbound descriptor. cbuffer16_vs has NO vertex fetch, so
        // this isolates the CB path (no vertex SSBO). Expected values are hand-reasoned.
        let vs_blob = corpus_sb("cbuffer16_vs");
        let ps_blob = corpus_sb("flat_color_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x2000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (ps_addr, cur) = place_aligned(&mut arena, base, cur, &ps_blob);
        // Constant-buffer data: a 16-dword (64-byte) block, 16-aligned. Its exact bytes do
        // not matter here (we assert the bind, not the values), only that the V# names it.
        let cb_off = (cur + 0xF) & !0xF;
        arena.resize(cb_off, 0);
        let cb_addr = base + cb_off as u64;
        arena.extend_from_slice(&[0xABu8; 64]);
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // The inline CB V#: base = cb_addr, stride 0 (flat), 16 records of dfmt-32 → the
        // decode gives byte_span = 4 bytes/element * 16 = 64 (matches the block).
        let cb_vsharp = {
            let w0 = (cb_addr & 0xFFFF_FFFF) as u32;
            // stride 0 in [29:16], base[47:32] in [15:0].
            let w1 = (cb_addr >> 32) as u32 & 0xFFFF;
            let w2 = 16u32; // num_records
            // dfmt=4 (_32, 1 comp of 4 bytes) [18:15], nfmt=7 (float) [14:12], identity swizzle.
            let w3: u32 = 4 | (5 << 3) | (6 << 6) | (7 << 9) | (7 << 12) | (4 << 15);
            let mut out = [0u8; 16];
            out[0..4].copy_from_slice(&w0.to_le_bytes());
            out[4..8].copy_from_slice(&w1.to_le_bytes());
            out[8..12].copy_from_slice(&w2.to_le_bytes());
            out[12..16].copy_from_slice(&w3.to_le_bytes());
            out
        };

        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, ps_addr);
        bind_vs_const_vsharp(&mut dcb, &cb_vsharp);
        dcb.extend_from_slice(&draw_auto(3));

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        let lists = sink.command_lists();
        assert_eq!(lists.len(), 1, "one submit → one command list");
        let cmds = &lists[0];

        // The pipeline must declare the const-buffer SSBO at set0/bind2 (the recompiler's
        // fixed CONST_BUFFER_SET/BINDING). It reads NO vertex buffer, so `storage` is None.
        let created = cmds
            .iter()
            .find_map(|c| match c {
                BackendCmd::CreatePipeline {
                    const_storage,
                    vertex_storage,
                    ..
                } => Some((*const_storage, vertex_storage.clone())),
                _ => None,
            })
            .expect("a CreatePipeline must be emitted");
        assert_eq!(
            created.0,
            Some(StorageBinding {
                set: 0,
                binding: 2,
                stride: 0
            }),
            "the VS's constant buffer is declared at set0/bind2"
        );
        assert!(
            created.1.is_empty(),
            "cbuffer16_vs fetches no vertex buffer, so there is no vertex-pull SSBO"
        );

        // A BindConstBuffer must point the set0/bind2 descriptor at the uploaded CB bytes.
        let bind = cmds
            .iter()
            .find_map(|c| match c {
                BackendCmd::BindConstBuffer { set, binding, id } => Some((*set, *binding, *id)),
                _ => None,
            })
            .expect("a BindConstBuffer must be emitted for the declared const buffer");
        assert_eq!((bind.0, bind.1), (0, 2), "const buffer bound at set0/bind2");

        // The CB bytes were pulled through the resource cache (create + upload of 64 bytes)
        // under the id the BindConstBuffer names — the descriptor is never left un-written.
        let bound_id = bind.2;
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                BackendCmd::CreateBuffer { id, size } if *id == bound_id && *size == 64
            )),
            "the 64-byte constant buffer is created under the bound id"
        );
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                BackendCmd::UploadBuffer { id, data, .. } if *id == bound_id && data.len() == 64
            )),
            "the constant buffer's bytes are uploaded"
        );
    }

    #[test]
    fn gcn_const_buffer_vs_defers_when_vsharp_null() {
        // Gap 2 strict-or-defer: a const-buffer-declaring VS whose CB V# is null/unbound
        // (user-SGPRs s[4:7] unprogrammed → base 0) defers the WHOLE draw — a pipeline with a
        // `const_storage` descriptor and no BindConstBuffer would leave that descriptor
        // un-written. No CreatePipeline / draw is emitted.
        let vs_blob = corpus_sb("cbuffer16_vs");
        let ps_blob = corpus_sb("flat_color_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x1000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (ps_addr, _cur) = place_aligned(&mut arena, base, cur, &ps_blob);

        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, ps_addr);
        // NO bind_vs_const_vsharp — s[4:7] stays all-zero → a null CB V#.
        dcb.extend_from_slice(&draw_auto(3));

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        // The draw deferred cleanly: no command list shipped (no CreatePipeline, no draw).
        assert!(
            sink.command_lists().is_empty(),
            "a null constant-buffer V# must defer the whole draw, emitting nothing"
        );
    }

    // ---- descriptor-provenance consumption (task-130 slices 2-4) ---------------

    /// Program a stage's user-SGPR block directly on a `GpuState` (bypassing the DCB walk):
    /// `words[i]` lands at `SPI_SHADER_USER_DATA_<stage>_0 + i`. Lets the provenance tests
    /// drive `derive_*` against arbitrary SGPR layouts without a corpus shader.
    fn program_user_sgprs(state: &mut GpuState, stage: Stage, words: &[(usize, u32)]) {
        use crate::pm4::opcodes::sh_reg;
        let base = match stage {
            Stage::Vertex => sh_reg::SPI_SHADER_USER_DATA_VS_0,
            Stage::Pixel => sh_reg::SPI_SHADER_USER_DATA_PS_0,
        };
        for &(slot, val) in words {
            state.sh_regs.set(base + slot as u32, val);
        }
    }

    /// The four dwords of an inline constant-buffer V# naming `base` with `num_records`
    /// dword-32 elements (dfmt=4/nfmt=7, identity swizzle) — the same encoding the CB
    /// end-to-end test hand-lays, extracted so the provenance unit tests reuse it.
    fn inline_cb_vsharp(base: u64, num_records: u32) -> [u32; 4] {
        let w0 = (base & 0xFFFF_FFFF) as u32;
        let w1 = (base >> 32) as u32 & 0xFFFF;
        let w2 = num_records;
        let w3: u32 = 4 | (5 << 3) | (6 << 6) | (7 << 9) | (7 << 12) | (4 << 15);
        [w0, w1, w2, w3]
    }

    /// Run `derive_const_buffer` against a state whose SGPRs the test programmed, with no
    /// draw/present activity — a bare Executor is enough (the CB V# is read from registers,
    /// not memory).
    fn derive_const_buffer_with(
        state: &mut GpuState,
        source: ps4_gcn::DescriptorSource,
    ) -> Option<crate::vbuf::BufferRange> {
        derive_const_buffer_stage(state, source, Stage::Vertex)
    }

    /// Like [`derive_const_buffer_with`] but resolves the V# from `stage`'s user-SGPR block
    /// (the VS-block helper above delegates here with `Stage::Vertex`). Covers the PS-stage
    /// const-buffer path (Celeste's pixel-shader `s_buffer_load`, task-139).
    fn derive_const_buffer_stage(
        state: &mut GpuState,
        source: ps4_gcn::DescriptorSource,
        stage: Stage,
    ) -> Option<crate::vbuf::BufferRange> {
        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let exec = Executor::new(
            ExecMode::Draw,
            &sink,
            state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        exec.derive_const_buffer(source, stage)
    }

    #[test]
    fn derive_const_buffer_reads_from_source_sgpr_not_hardcoded_four() {
        // The CB V# is resolved from the binding's provenance SGPR, NOT a hardcoded s[4:7].
        // Program a REAL V# at s[8:11] and a DIFFERENT one at the old-hardcoded s[4:7]; with
        // source = InlineVSharp{8} the resolved range must name the s[8:11] buffer. This
        // FAILS against the deleted `CONST_BUFFER_SBASE_SGPR = 4` (which would read s[4:7]).
        const WANT_BASE: u64 = 0x9000;
        const DECOY_BASE: u64 = 0x1000;
        let mut state = GpuState::default();
        // Decoy at s[4:7] (what the old hardcoded const would have read).
        let decoy = inline_cb_vsharp(DECOY_BASE, 8);
        program_user_sgprs(
            &mut state,
            Stage::Vertex,
            &[(4, decoy[0]), (5, decoy[1]), (6, decoy[2]), (7, decoy[3])],
        );
        // The real V# at s[8:11] — the source names this quad.
        let want = inline_cb_vsharp(WANT_BASE, 16);
        program_user_sgprs(
            &mut state,
            Stage::Vertex,
            &[(8, want[0]), (9, want[1]), (10, want[2]), (11, want[3])],
        );

        let range = derive_const_buffer_with(
            &mut state,
            ps4_gcn::DescriptorSource::InlineVSharp { sgpr: 8 },
        )
        .expect("CB V# at s[8:11] resolves");
        assert_eq!(
            range.addr, WANT_BASE,
            "the CB base must come from the source SGPR quad s[8:11], not the hardcoded s[4:7]"
        );
        assert_ne!(
            range.addr, DECOY_BASE,
            "resolving must not read the old hardcoded s[4:7] slot"
        );
    }

    #[test]
    fn derive_const_buffer_reads_declaring_stage_block() {
        // task-139: a PS `s_buffer_load` const buffer lives in the PIXEL user-SGPR block, a
        // VS one in the VERTEX block. Program a REAL V# at the PS block's s[8:11] and a DECOY
        // at the VS block's s[8:11]; resolving with Stage::Pixel must read the PS block, and
        // with Stage::Vertex the VS block — proving the stage selects the SGPR file.
        const PS_BASE: u64 = 0xA000;
        const VS_BASE: u64 = 0xB000;
        let mut state = GpuState::default();
        let ps_v = inline_cb_vsharp(PS_BASE, 16);
        program_user_sgprs(
            &mut state,
            Stage::Pixel,
            &[(8, ps_v[0]), (9, ps_v[1]), (10, ps_v[2]), (11, ps_v[3])],
        );
        let vs_v = inline_cb_vsharp(VS_BASE, 16);
        program_user_sgprs(
            &mut state,
            Stage::Vertex,
            &[(8, vs_v[0]), (9, vs_v[1]), (10, vs_v[2]), (11, vs_v[3])],
        );

        let ps_range = derive_const_buffer_stage(
            &mut state,
            ps4_gcn::DescriptorSource::InlineVSharp { sgpr: 8 },
            Stage::Pixel,
        )
        .expect("PS CB V# at PS-block s[8:11] resolves");
        assert_eq!(
            ps_range.addr, PS_BASE,
            "a PS const buffer must be read from the PIXEL user-SGPR block, not the VERTEX one"
        );

        let vs_range = derive_const_buffer_stage(
            &mut state,
            ps4_gcn::DescriptorSource::InlineVSharp { sgpr: 8 },
            Stage::Vertex,
        )
        .expect("VS CB V# at VS-block s[8:11] resolves");
        assert_eq!(
            vs_range.addr, VS_BASE,
            "a VS const buffer must be read from the VERTEX user-SGPR block"
        );
    }

    #[test]
    fn derive_const_buffer_out_of_range_sgpr_defers() {
        // strict-or-defer (doc-6 Entry 10): a source whose SGPR quad runs past the 16-slot
        // user block (s[14:17]) cannot be a bound CB — defer cleanly, never a partial bind.
        let mut state = GpuState::default();
        let v = inline_cb_vsharp(0x9000, 16);
        // Program what we can (s[14], s[15]); s[16]/s[17] do not exist.
        program_user_sgprs(&mut state, Stage::Vertex, &[(14, v[0]), (15, v[1])]);
        assert!(
            derive_const_buffer_with(
                &mut state,
                ps4_gcn::DescriptorSource::InlineVSharp { sgpr: 14 },
            )
            .is_none(),
            "an out-of-range CB SBASE must defer the draw"
        );
    }

    #[test]
    fn derive_const_buffer_set_pointer_source_defers() {
        // A SetPointer is the vertex-fetch ABI, not the const-buffer ABI (the CB V# is
        // inline). A CB binding carrying a SetPointer source is malformed → defer.
        let mut state = GpuState::default();
        let v = inline_cb_vsharp(0x9000, 16);
        program_user_sgprs(
            &mut state,
            Stage::Vertex,
            &[(4, v[0]), (5, v[1]), (6, v[2]), (7, v[3])],
        );
        assert!(
            derive_const_buffer_with(
                &mut state,
                ps4_gcn::DescriptorSource::SetPointer {
                    sgpr: 4,
                    desc_offset: 0,
                },
            )
            .is_none(),
            "a SetPointer CB source is not the const-buffer ABI — defer"
        );
    }

    #[test]
    fn texture_slot_of_corpus_inline_matches_the_fixed_corpus_abi() {
        // The corpus PS resolves T# InlineVSharp{0}, S# at ssamp s[8] (s_offset = 8). The
        // provenance-built slot must equal the old fixed CORPUS_TEXTURE_SLOT: pointer in
        // s[0:1], T# at byte 0, S# at byte 32 (== T_SHARP_SIZE, the (8-0)*4 SGPR distance).
        let binding = ps4_gcn::SamplerBinding {
            set: 0,
            binding: 1,
            source: ps4_gcn::DescriptorSource::InlineVSharp { sgpr: 0 },
            s_offset: 8,
        };
        let slot = texture_slot_of(&binding).expect("corpus texture slot resolves");
        assert_eq!(slot.user_sgpr, 0, "descriptor-set pointer in s[0:1]");
        assert_eq!(slot.t_offset, 0, "T# at set offset 0");
        assert_eq!(
            slot.s_offset,
            crate::vbuf::T_SHARP_SIZE as u64,
            "S# right after the T# (byte 32)"
        );
    }

    #[test]
    fn texture_slot_of_shifted_sgprs_tracks_the_provenance() {
        // A shifted-SGPR texture: the descriptor-set pointer is in s[4:5] (SetPointer with a
        // desc_offset), and the S# block is at s[12]. The slot must track the provenance, NOT
        // a hardcoded s[0:1]/offset-0. This FAILS against the deleted CORPUS_TEXTURE_SLOT.
        let binding = ps4_gcn::SamplerBinding {
            set: 0,
            binding: 1,
            source: ps4_gcn::DescriptorSource::SetPointer {
                sgpr: 4,
                desc_offset: 64,
            },
            s_offset: 12,
        };
        let slot = texture_slot_of(&binding).expect("shifted texture slot resolves");
        assert_eq!(
            slot.user_sgpr, 4,
            "pointer pair from the SetPointer SBASE s[4:5]"
        );
        assert_eq!(slot.t_offset, 64, "T# at the SetPointer desc_offset");
        // S# byte offset = t_offset + (s_sgpr - t_sgpr)*4 = 64 + (12 - 4)*4 = 96.
        assert_eq!(slot.s_offset, 96, "S# byte offset from the SGPR distance");
    }

    #[test]
    fn texture_slot_of_s_before_t_defers() {
        // strict-or-defer: an S# SGPR block that sits BEFORE the T#'s quad cannot be a valid
        // contiguous descriptor set — defer, never a partial/underflowed bind.
        let binding = ps4_gcn::SamplerBinding {
            set: 0,
            binding: 1,
            source: ps4_gcn::DescriptorSource::InlineVSharp { sgpr: 8 },
            s_offset: 0,
        };
        assert!(
            texture_slot_of(&binding).is_none(),
            "an S# SGPR before the T# quad must defer"
        );
    }

    #[test]
    fn derive_texture_binding_reads_through_shifted_sgpr_provenance() {
        // End-to-end through the bounded seam with a SHIFTED texture ABI: the descriptor-set
        // pointer lives in s[4:5] (a SetPointer), the T# at set offset 64, the S# at s[12]
        // (byte offset 96). The resolved texture must name the T# reached via that shifted
        // pointer — proving `derive_texture_binding` honors provenance, not a fixed s[0:1]/0.
        const SET_OFF: usize = 0x100; // descriptor set placed at arena offset 0x100 (aligned)
        const T_IN_SET: usize = 64; // T# 64 bytes into the set (matches desc_offset)
        const S_IN_SET: usize = 96; // S# at (12-4)*4 = 96 bytes into the set

        // Reserve up front so `place_aligned`'s appends never reallocate (guest==host addr
        // holds only if `base` is stable). Lay the set region first, then 256-aligned texels.
        let mut arena: Vec<u8> = Vec::with_capacity(0x1000);
        arena.resize(SET_OFF + S_IN_SET + 16, 0);
        let base = arena.as_ptr() as u64;
        let set_ptr = base + SET_OFF as u64;
        // Texel data placed at a 256-aligned GUEST address (T# base = addr>>8 round-trips).
        let cursor = arena.len();
        let (tex_addr, _cur) = place_aligned(&mut arena, base, cursor, &[0u8; 16]);
        // Fill the set: T# at +64 naming the texels, S# (point filter) at +96.
        arena[SET_OFF + T_IN_SET..SET_OFF + T_IN_SET + 32]
            .copy_from_slice(&linear_tsharp(tex_addr, 2, 2));
        arena[SET_OFF + S_IN_SET..SET_OFF + S_IN_SET + 16].copy_from_slice(&point_ssharp());
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let mut state = GpuState::default();
        // Descriptor-set pointer in s[4:5] (the SetPointer SBASE).
        program_user_sgprs(
            &mut state,
            Stage::Pixel,
            &[
                (4, (set_ptr & 0xFFFF_FFFF) as u32),
                (5, (set_ptr >> 32) as u32),
            ],
        );

        let binding = ps4_gcn::SamplerBinding {
            set: 0,
            binding: 1,
            source: ps4_gcn::DescriptorSource::SetPointer {
                sgpr: 4,
                desc_offset: T_IN_SET as u32,
            },
            s_offset: 12,
        };

        let sink = MockSink::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        let resolved = exec
            .derive_texture_binding(&binding)
            .expect("texture resolves through the shifted pointer");
        assert_eq!(
            resolved.texture.base, tex_addr,
            "the T# reached via the s[4:5] pointer at set offset 64 names the texel data"
        );
        assert_eq!(
            (resolved.texture.width, resolved.texture.height),
            (2, 2),
            "the shifted T# decodes the 2×2 extent"
        );
    }

    /// Build the eight little-endian dwords of a linear 2×2 RGBA8 T# (image resource):
    /// word0 = base>>8, word2 = (w-1) | ((h-1)<<14), tiling 0. Hand-laid so the test's
    /// expectations do not come from `decode_t_sharp`.
    fn linear_tsharp(base: u64, w: u32, h: u32) -> [u8; 32] {
        assert_eq!(base & 0xFF, 0, "T# base must be 256-aligned");
        let mut words = [0u32; 8];
        // base[39:8] in word0, base[47:40] in word1[7:0] (48-bit HLE base), dfmt [25:20].
        words[0] = (base >> 8) as u32;
        words[1] = ((base >> 40) as u32 & 0xFF) | (10 << 20);
        words[2] = (w - 1) | ((h - 1) << 14);
        let mut out = [0u8; 32];
        for (i, wv) in words.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&wv.to_le_bytes());
        }
        out
    }

    /// Build the four dwords of a point-filter S# (128-bit sampler): filter bit clear.
    fn point_ssharp() -> [u8; 16] {
        [0u8; 16]
    }

    /// Program the PS user-SGPRs with the INLINE T#/S# descriptors (the corpus
    /// `DescriptorSource::InlineVSharp` ABI, task-149): the launch ABI loads the 256-bit T#
    /// straight into `s[0:7]` and the 128-bit S# into `s[8:11]` — no descriptor-set pointer,
    /// no memory dereference. `t` is the 32-byte T#, `s` the 16-byte S#.
    fn bind_ps_inline_texture(dcb: &mut Vec<u32>, t: &[u8; 32], s: &[u8; 16]) {
        let sh = |abs: u32| abs - reg_base_sh();
        let mut words = [0u32; 12];
        for (i, w) in words.iter_mut().enumerate().take(8) {
            *w = u32::from_le_bytes([t[i * 4], t[i * 4 + 1], t[i * 4 + 2], t[i * 4 + 3]]);
        }
        for (i, w) in words.iter_mut().enumerate().skip(8) {
            let j = (i - 8) * 4;
            *w = u32::from_le_bytes([s[j], s[j + 1], s[j + 2], s[j + 3]]);
        }
        dcb.extend(set_reg(
            op::IT_SET_SH_REG,
            sh(sh_reg::SPI_SHADER_USER_DATA_PS_0),
            &words,
        ));
    }

    #[test]
    fn gcn_textured_draw_end_to_end_emits_image_sampler_bind_and_single_reupload() {
        // AC #2 (textured milestone): a DCB that binds passthrough_vs (SSBO-fetch VS) +
        // texture_sample_ps (the image_sample corpus PS), programs the VS descriptor-set
        // pointer (s[2:3] → a vec4 V#) AND the PS descriptor-set pointer (s[0:1] → a T# +
        // S#), and issues DRAW_INDEX_AUTO. The executor recompiles both .sb through the
        // chain and emits the full textured sequence: the pipeline declares texture Some,
        // and a CreateImage / UploadImage(detiled) / CreateSampler / BindTexture appears.
        // Every expected value is an independently-reasoned literal.
        //
        // A guest write to the texel range then re-uploads the image EXACTLY once on the
        // next submit (the DirtySource's first real texture payoff) — asserted by a second
        // run that emits one UploadImage and no second CreateImage.

        let vs_blob = corpus_sb("passthrough_vs");
        let ps_blob = corpus_sb("texture_sample_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x4000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (ps_addr, cur) = place_aligned(&mut arena, base, cur, &ps_blob);
        // Vertex data: 3 vec4 vertices (48 bytes), 16-aligned.
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        // VS descriptor set: one vec4 V# → base=vtx_addr, stride 16, 3 records.
        let vs_desc_off = arena.len();
        let vs_desc_ptr = base + vs_desc_off as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        // Texel data: a 2×2 RGBA8 texture, placed 256-aligned (T# base = addr>>8).
        let tex_cursor = arena.len();
        let (tex_addr, _cur) = place_aligned(&mut arena, base, tex_cursor, &[0u8; 16]);
        // The corpus PS's T#/S# are INLINE in the user SGPRs (DescriptorSource::InlineVSharp,
        // task-149): the launch ABI loads the 32-byte T# into s[0:7] and the 16-byte S# into
        // s[8:11] directly — no descriptor-set pointer, no guest-memory dereference.
        let ps_tsharp = linear_tsharp(tex_addr, 2, 2);
        let ps_ssharp = point_ssharp();
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // DCB: shader binds + VS desc-set + inline PS T#/S# + DRAW_INDEX_AUTO.
        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, ps_addr);
        bind_vs_desc_set(&mut dcb, vs_desc_ptr);
        bind_ps_inline_texture(&mut dcb, &ps_tsharp, &ps_ssharp);
        dcb.extend_from_slice(&draw_auto(3));

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        let lists = sink.command_lists();
        assert_eq!(lists.len(), 1, "one submit → one command list");
        let cmds = &lists[0];

        // The pipeline must declare exactly one combined image-sampler binding: this PS
        // samples a single texture, so the multi-texture path (task-199) must not have
        // grown its layout.
        let tex_binding = match &cmds[0] {
            BackendCmd::CreatePipeline { textures, .. } => {
                assert_eq!(
                    textures.len(),
                    1,
                    "a single-texture PS declares exactly one combined image-sampler"
                );
                textures[0]
            }
            other => panic!("cmds[0] must be CreatePipeline, got {other:?}"),
        };
        // The recompiler placed the sampler at set 0, binding 1 (clear of the VS SSBO at
        // binding 0). Hand-reasoned from the recompiler's fixed PS_TEXTURE_SET/BINDING.
        assert_eq!(tex_binding, TextureBinding { set: 0, binding: 1 });

        // Exactly one CreateImage(2×2, RGBA8), one UploadImage, one CreateSampler(point),
        // one BindTexture naming the image + sampler at (set 0, binding 1).
        let create_image: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                BackendCmd::CreateImage {
                    id,
                    width,
                    height,
                    format,
                } => Some((*id, *width, *height, *format)),
                _ => None,
            })
            .collect();
        assert_eq!(
            create_image.len(),
            1,
            "exactly one CreateImage for the texture"
        );
        let (image_id, w, h, fmt) = create_image[0];
        assert_eq!((w, h), (2, 2), "2×2 texture");
        assert_eq!(fmt, TextureFormat::R8G8B8A8Unorm);

        let upload_image: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, BackendCmd::UploadImage { .. }))
            .collect();
        assert_eq!(
            upload_image.len(),
            1,
            "exactly one UploadImage on first use"
        );

        let create_sampler: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                BackendCmd::CreateSampler { id, desc } => Some((*id, *desc)),
                _ => None,
            })
            .collect();
        assert_eq!(create_sampler.len(), 1, "exactly one CreateSampler");
        let (sampler_id, sdesc) = create_sampler[0];
        // Point S# (all-zero word0) → Nearest filter, WRAP (Repeat) on both axes. Hand-reasoned.
        assert_eq!(sdesc.mag_filter, SamplerFilter::Nearest);
        assert_eq!(sdesc.min_filter, SamplerFilter::Nearest);
        assert_eq!(sdesc.address_mode_u, SamplerAddressMode::Repeat);
        assert_eq!(sdesc.address_mode_v, SamplerAddressMode::Repeat);

        let bind_tex: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                BackendCmd::BindTexture {
                    set,
                    binding,
                    image_id,
                    sampler_id,
                } => Some((*set, *binding, *image_id, *sampler_id)),
                _ => None,
            })
            .collect();
        assert_eq!(bind_tex.len(), 1, "exactly one BindTexture");
        assert_eq!(
            bind_tex[0],
            (0, 1, image_id, sampler_id),
            "BindTexture names the created image + sampler at (set 0, binding 1)"
        );

        // Second submit WITHOUT a guest write: a clean hit → NO image commands (the image
        // and sampler already exist, the entry is clean).
        let sink2 = MockSink::default();
        let mut exec2 = Executor::new(
            ExecMode::Draw,
            &sink2,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec2.run(&range_over(&dcb, false)) };
        let cmds2 = &sink2.command_lists()[0];
        assert!(
            !cmds2.iter().any(|c| matches!(
                c,
                BackendCmd::CreateImage { .. } | BackendCmd::UploadImage { .. }
            )),
            "clean reuse re-uploads nothing"
        );
        // The texture is still bound (a clean hit reuses the same image id).
        assert!(
            cmds2.iter().any(|c| matches!(
                c,
                BackendCmd::BindTexture { image_id: iid, .. } if *iid == image_id
            )),
            "the cached texture is re-bound on reuse"
        );

        // Now model a guest write to the texel range (the DirtySource → cache
        // invalidation the memory manager fires on a write), then a third submit →
        // EXACTLY one re-upload (UploadImage) and NO second CreateImage.
        resources.invalidate_range(tex_addr, 16);
        let sink3 = MockSink::default();
        let mut exec3 = Executor::new(
            ExecMode::Draw,
            &sink3,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec3.run(&range_over(&dcb, false)) };
        let cmds3 = &sink3.command_lists()[0];
        let reuploads = cmds3
            .iter()
            .filter(|c| matches!(c, BackendCmd::UploadImage { .. }))
            .count();
        let recreates = cmds3
            .iter()
            .filter(|c| matches!(c, BackendCmd::CreateImage { .. }))
            .count();
        assert_eq!(reuploads, 1, "guest write → exactly one image re-upload");
        assert_eq!(
            recreates, 0,
            "no second CreateImage — the image already exists"
        );
    }

    /// Program the CONTEXT bank's `CB_COLOR0_*` registers for an OFFSCREEN render target via
    /// SET_CONTEXT_REG (the register route `derive_target` reads): an unregistered base,
    /// COLOR_8_8_8_8 format, linear tiling, and PITCH/SLICE tile-max encodings giving the
    /// extent. `pitch`/`height` are the desired pixel dimensions (both multiples of 8 so the
    /// tile-max encodings are exact).
    fn set_offscreen_rt_regs(dcb: &mut Vec<u32>, rt_base: u64, pitch: u32, height: u32) {
        use crate::pm4::opcodes::context_reg as ctx;
        use crate::pm4::opcodes::reg_base;
        let ctxoff = |abs: u32| abs - reg_base::CONTEXT;
        // CB_COLOR0_BASE in 256-byte units.
        dcb.extend(set_reg(
            op::IT_SET_CONTEXT_REG,
            ctxoff(ctx::CB_COLOR0_BASE),
            &[(rt_base >> 8) as u32],
        ));
        // FORMAT = COLOR_8_8_8_8 (0x0A) in bits [5:2].
        dcb.extend(set_reg(
            op::IT_SET_CONTEXT_REG,
            ctxoff(ctx::CB_COLOR0_INFO),
            &[0x0A << 2],
        ));
        // ATTRIB tile-mode index 0 → linear.
        dcb.extend(set_reg(
            op::IT_SET_CONTEXT_REG,
            ctxoff(ctx::CB_COLOR0_ATTRIB),
            &[0],
        ));
        // PITCH tile-max = pitch/8 − 1.
        dcb.extend(set_reg(
            op::IT_SET_CONTEXT_REG,
            ctxoff(ctx::CB_COLOR0_PITCH),
            &[pitch / 8 - 1],
        ));
        // SLICE tile-max = pitch*height/64 − 1.
        dcb.extend(set_reg(
            op::IT_SET_CONTEXT_REG,
            ctxoff(ctx::CB_COLOR0_SLICE),
            &[(pitch * height) / 64 - 1],
        ));
    }

    /// Set only `CB_COLOR0_BASE` (used to switch the consumer draw back to the videoout
    /// framebuffer after the producer left the RT base programmed).
    fn set_cb_color0_base(dcb: &mut Vec<u32>, base: u64) {
        use crate::pm4::opcodes::context_reg as ctx;
        use crate::pm4::opcodes::reg_base;
        let ctxoff = |abs: u32| abs - reg_base::CONTEXT;
        dcb.extend(set_reg(
            op::IT_SET_CONTEXT_REG,
            ctxoff(ctx::CB_COLOR0_BASE),
            &[(base >> 8) as u32],
        ));
    }

    #[test]
    fn rt_as_texture_producer_then_consumer_binds_rt_with_zero_image_uploads() {
        // AC #1 (executor seam): a draw-to-RT (producer) followed by a draw that SAMPLES the
        // RT's guest range (consumer) resolves the sampled bind to the render target
        // host-side. Assertions (all independently reasoned):
        //   * EXACTLY one CreateRenderTarget for the RT range, and no upload paired with it;
        //   * the consumer's BindTexture.image_id == the RT's ResourceId;
        //   * ZERO CreateImage / UploadImage for that range (the RT is GPU-filled, never
        //     detiled from guest bytes).
        use crate::pm4::opcodes::context_reg as ctx;
        use ps4_core::gpu::{DisplayBuffer, DisplayBufferSource, registered_display_buffers};

        // Wire a videoout framebuffer so the CONSUMER draw has a registered (videoout) target
        // to render into — otherwise its own CB_COLOR0_BASE would be read as a second RT.
        struct OneBuffer(DisplayBuffer);
        impl DisplayBufferSource for OneBuffer {
            fn lookup(&self, base: u64) -> Option<DisplayBuffer> {
                (self.0.base == base).then_some(self.0)
            }
        }
        let fb_base = 0xC000_0000u64;
        let dbsrc: Arc<dyn DisplayBufferSource> = Arc::new(OneBuffer(DisplayBuffer {
            base: fb_base,
            width: 1920,
            height: 1080,
        }));
        let _dbguard = registered_display_buffers().override_scoped(dbsrc);

        // The offscreen RT lives at an UNREGISTERED, 256-aligned base (not the framebuffer).
        // Its bytes are never in the arena — the RT path never reads them.
        let rt_base = 0xD000_0000u64;
        let rt_pitch = 8u32;
        let rt_height = 8u32;
        let rt_size = (rt_pitch * rt_height * 4) as u64; // 256 bytes, RGBA8.

        // Arena: producer VS/PS (passthrough + flat_color), consumer PS (texture_sample),
        // shared vertex data + VS desc set, and the consumer PS desc set (T# at rt_base + S#).
        let vs_blob = corpus_sb("passthrough_vs");
        let flat_ps_blob = corpus_sb("flat_color_ps");
        let tex_ps_blob = corpus_sb("texture_sample_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x8000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (flat_ps_addr, cur) = place_aligned(&mut arena, base, cur, &flat_ps_blob);
        let (tex_ps_addr, cur) = place_aligned(&mut arena, base, cur, &tex_ps_blob);
        // Vertex data: 3 vec4 vertices (48 bytes), 16-aligned.
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        // VS descriptor set: one vec4 V#.
        let vs_desc_off = arena.len();
        let vs_desc_ptr = base + vs_desc_off as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        // Consumer PS T#/S# are INLINE in the user SGPRs (DescriptorSource::InlineVSharp,
        // task-149): the 2×2 T#'s base is the RT base (so the sampled range
        // [rt_base, rt_base+16) is fully contained in the RT range), the S# is a point sampler.
        let consumer_tsharp = linear_tsharp(rt_base, 2, 2);
        let consumer_ssharp = point_ssharp();
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // DCB. Producer: shaders + VS desc set + offscreen RT regs + draw. Consumer:
        // shaders (texture PS) + VS desc set + inline PS T#/S# + switch RT back to fb + draw.
        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, flat_ps_addr);
        bind_vs_desc_set(&mut dcb, vs_desc_ptr);
        set_offscreen_rt_regs(&mut dcb, rt_base, rt_pitch, rt_height);
        dcb.extend_from_slice(&draw_auto(3));

        bind_gcn_shaders(&mut dcb, vs_addr, tex_ps_addr);
        bind_vs_desc_set(&mut dcb, vs_desc_ptr);
        bind_ps_inline_texture(&mut dcb, &consumer_tsharp, &consumer_ssharp);
        set_cb_color0_base(&mut dcb, fb_base); // consumer renders into videoout
        dcb.extend_from_slice(&draw_auto(3));

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        let lists = sink.command_lists();
        assert_eq!(lists.len(), 1, "one submit → one command list");
        let cmds = &lists[0];

        // Sanity: the offscreen RT geometry the producer derived matches the regs above (so a
        // wrong SLICE/PITCH decode would surface here, not as a silent mis-sized RT).
        let derived = {
            let mut s = GpuState::default();
            s.ctx_regs.set(ctx::CB_COLOR0_BASE, (rt_base >> 8) as u32);
            s.ctx_regs.set(ctx::CB_COLOR0_INFO, 0x0A << 2);
            s.ctx_regs.set(ctx::CB_COLOR0_PITCH, rt_pitch / 8 - 1);
            s.ctx_regs
                .set(ctx::CB_COLOR0_SLICE, (rt_pitch * rt_height) / 64 - 1);
            crate::derive::derive_target(&s).expect("offscreen RT derives")
        };
        assert_eq!((derived.width, derived.height), (rt_pitch, rt_height));
        assert_eq!(
            derived.kind,
            ps4_core::gpu::TargetKind::Offscreen {
                base: rt_base,
                size: rt_size,
            }
        );

        // EXACTLY one CreateRenderTarget for the RT (8×8, RGBA8).
        let create_rt: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                BackendCmd::CreateRenderTarget {
                    id,
                    width,
                    height,
                    format,
                } => Some((*id, *width, *height, *format)),
                _ => None,
            })
            .collect();
        assert_eq!(
            create_rt.len(),
            1,
            "exactly one CreateRenderTarget for the offscreen RT"
        );
        let (rt_id, w, h, fmt) = create_rt[0];
        assert_eq!((w, h), (rt_pitch, rt_height), "RT extent from PITCH/SLICE");
        assert_eq!(fmt, ColorFormat::R8G8B8A8Unorm);

        // ZERO CreateImage / UploadImage anywhere — the RT is GPU-filled, and the consumer
        // binds it host-side rather than detiling guest bytes (the whole point of AC #1).
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, BackendCmd::CreateImage { .. }))
                .count(),
            0,
            "no CreateImage — an RT-as-texture bind never creates a sampled image"
        );
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, BackendCmd::UploadImage { .. }))
                .count(),
            0,
            "no UploadImage — an RT's texels come from the GPU, never a guest upload"
        );

        // task-56 step 4: exactly one SetRenderTarget names the RT — the pass-boundary signal
        // that tells the backend to record the producer draw INTO the RT (not videoout). It
        // is emitted for the producer whether the CreateRenderTarget fired or was a cache hit.
        let set_rt: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                BackendCmd::SetRenderTarget { id } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(
            set_rt.len(),
            1,
            "exactly one SetRenderTarget for the offscreen producer draw"
        );
        assert_eq!(
            set_rt[0], rt_id,
            "SetRenderTarget names the render target the producer draws into"
        );

        // task-56 step 4 ordering invariant: SetRenderTarget precedes the FIRST draw (the
        // producer, into the RT); the SECOND draw (the consumer, into videoout) has NO
        // SetRenderTarget before it. This is what lets the backend segment the submit into an
        // offscreen producer pass then the videoout pass — the present latch is armed only by
        // the videoout draw.
        let draw_positions: Vec<usize> = cmds
            .iter()
            .enumerate()
            .filter_map(|(i, c)| matches!(c, BackendCmd::DrawAuto { .. }).then_some(i))
            .collect();
        assert_eq!(draw_positions.len(), 2, "producer draw + consumer draw");
        let set_rt_pos = cmds
            .iter()
            .position(|c| matches!(c, BackendCmd::SetRenderTarget { .. }))
            .expect("a SetRenderTarget was emitted");
        assert!(
            set_rt_pos < draw_positions[0],
            "SetRenderTarget precedes the producer draw"
        );
        let set_rts_before_consumer = cmds[draw_positions[0] + 1..draw_positions[1]]
            .iter()
            .filter(|c| matches!(c, BackendCmd::SetRenderTarget { .. }))
            .count();
        assert_eq!(
            set_rts_before_consumer, 0,
            "the consumer (videoout) draw has no SetRenderTarget — it targets videoout"
        );

        // The consumer's BindTexture names the RT's ResourceId (host-side render-to-texture).
        let bind_tex: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                BackendCmd::BindTexture { image_id, .. } => Some(*image_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            bind_tex.len(),
            1,
            "exactly one BindTexture (the consumer's)"
        );
        assert_eq!(
            bind_tex[0], rt_id,
            "the sampled bind resolves to the render target's id"
        );
    }

    #[test]
    fn rt_readback_policy_off_emits_none_all_emits_one_and_leaves_entry_clean() {
        // AC #2 (task-56 step 5): the opt-in RT readback lever. A single producer draw into
        // an offscreen RT, run twice through the SAME env-resolved policy seam:
        //   * UNEMUPS4_RT_READBACK Off (unset)  → ZERO ReadbackRenderTarget commands;
        //   * UNEMUPS4_RT_READBACK All ("all")   → EXACTLY one, naming the RT id + guest range,
        //     and the RT cache entry stays CLEAN (a re-render is a cache hit — no second
        //     CreateRenderTarget, no upload — because a readback never dirties the RT).
        // This test alone owns the env var (single-owner, mirrors pm4::trace::env_gate) so
        // there is no cross-test race on the process-global lever.
        use ps4_core::gpu::registered_display_buffers;
        use ps4_core::gpu::{DisplayBuffer, DisplayBufferSource};

        // A registered videoout framebuffer so the RT base is unambiguously the offscreen
        // target (the consumer half of the AC#1 harness is unnecessary here — one producer
        // draw is enough to exercise the readback path).
        let fb_base = 0xC000_0000u64;
        struct OneBuffer(DisplayBuffer);
        impl DisplayBufferSource for OneBuffer {
            fn lookup(&self, base: u64) -> Option<DisplayBuffer> {
                (self.0.base == base).then_some(self.0)
            }
        }
        let dbsrc: Arc<dyn DisplayBufferSource> = Arc::new(OneBuffer(DisplayBuffer {
            base: fb_base,
            width: 1920,
            height: 1080,
        }));
        let _dbguard = registered_display_buffers().override_scoped(dbsrc);

        // Offscreen RT at an unregistered 256-aligned base (never in the arena — the RT path
        // reads no guest bytes). 8×8 RGBA8 = 256 bytes.
        let rt_base = 0xD000_0000u64;
        let rt_pitch = 8u32;
        let rt_height = 8u32;
        let rt_size = (rt_pitch * rt_height * 4) as u64;

        // Arena: passthrough VS + flat_color PS + a vec4 vertex buffer + its V# desc set.
        let vs_blob = corpus_sb("passthrough_vs");
        let flat_ps_blob = corpus_sb("flat_color_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x4000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (flat_ps_addr, cur) = place_aligned(&mut arena, base, cur, &flat_ps_blob);
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        let vs_desc_off = arena.len();
        let vs_desc_ptr = base + vs_desc_off as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // DCB: bind shaders + VS desc set + offscreen RT regs + one draw into the RT.
        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, flat_ps_addr);
        bind_vs_desc_set(&mut dcb, vs_desc_ptr);
        set_offscreen_rt_regs(&mut dcb, rt_base, rt_pitch, rt_height);
        dcb.extend_from_slice(&draw_auto(3));

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        // Run one submit under whatever UNEMUPS4_RT_READBACK is currently set to, returning the
        // shipped command list. A fresh cache per call so the first run's create doesn't mask
        // the second's clean-hit assertion (the RT-clean check runs on its own fresh cache).
        let run_once = || {
            let sink = MockSink::default();
            let mut state = GpuState::default();
            let embedded = EmbeddedShaderProvider::new();
            let gcn = crate::shader::gcn::GcnShaderProvider::new();
            let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
            let chain = ChainProvider::new(&providers);
            let mut pipelines = PipelineCache::new();
            let mut resources = ResourceCache::new();
            let mut exec = Executor::new(
                ExecMode::Draw,
                &sink,
                &mut state,
                &chain,
                &mut pipelines,
                &mut resources,
            );
            unsafe { exec.run(&range_over(&dcb, false)) };
            let lists = sink.command_lists();
            assert_eq!(lists.len(), 1, "one submit → one command list");
            lists.into_iter().next().unwrap()
        };
        let readbacks =
            |cmds: &[BackendCmd]| -> Vec<(ResourceId, u64, u64, u32, ps4_core::gpu::Tiling)> {
                cmds.iter()
                    .filter_map(|c| match c {
                        BackendCmd::ReadbackRenderTarget {
                            id,
                            addr,
                            size,
                            pitch,
                            tiling,
                        } => Some((*id, *addr, *size, *pitch, *tiling)),
                        _ => None,
                    })
                    .collect()
            };
        let create_rts = |cmds: &[BackendCmd]| -> Vec<ResourceId> {
            cmds.iter()
                .filter_map(|c| match c {
                    BackendCmd::CreateRenderTarget { id, .. } => Some(*id),
                    _ => None,
                })
                .collect()
        };

        // --- Policy Off (env unset): zero readback commands. ---
        unsafe { std::env::remove_var("UNEMUPS4_RT_READBACK") };
        let off = run_once();
        assert!(
            readbacks(&off).is_empty(),
            "ReadbackPolicy::Off emits no ReadbackRenderTarget"
        );

        // --- Policy Off (explicit "off"): still zero. ---
        unsafe { std::env::set_var("UNEMUPS4_RT_READBACK", "off") };
        assert!(
            readbacks(&run_once()).is_empty(),
            "an explicit 'off' still emits no readback"
        );

        // --- Policy All: exactly one readback naming the RT id + guest range. ---
        unsafe { std::env::set_var("UNEMUPS4_RT_READBACK", "all") };
        let all = run_once();
        let rt_id = {
            let created = create_rts(&all);
            assert_eq!(
                created.len(),
                1,
                "exactly one CreateRenderTarget for the RT"
            );
            created[0]
        };
        let rb = readbacks(&all);
        assert_eq!(
            rb.len(),
            1,
            "ReadbackPolicy::All emits exactly one ReadbackRenderTarget for the flagged RT"
        );
        assert_eq!(
            rb[0],
            (
                rt_id,
                rt_base,
                rt_size,
                rt_pitch,
                ps4_core::gpu::Tiling::Linear
            ),
            "the readback names the RT's ResourceId, its guest range, and the GUEST surface's \
             row stride + tiling (task-181: the backend cannot pack a decodable readback from \
             the content extent alone)"
        );
        // The readback lands AFTER the producer draw (it must read a rendered RT).
        let draw_pos = all
            .iter()
            .position(|c| matches!(c, BackendCmd::DrawAuto { .. }))
            .expect("a producer draw was emitted");
        let rb_pos = all
            .iter()
            .position(|c| matches!(c, BackendCmd::ReadbackRenderTarget { .. }))
            .expect("a readback was emitted");
        assert!(
            rb_pos > draw_pos,
            "the readback follows the draw into the RT (reads a rendered target)"
        );

        // The RT cache entry stays CLEAN: on a fresh cache, create the RT then re-fetch it —
        // the second fetch is a clean hit (same id, no second CreateRenderTarget, no upload).
        // A readback is a GPU→CPU mirror; it never dirties the GPU-authored RT entry.
        {
            let mut cache = ResourceCache::new();
            let target = TargetDesc {
                width: rt_pitch,
                height: rt_height,
                kind: ps4_core::gpu::TargetKind::Offscreen {
                    base: rt_base,
                    size: rt_size,
                },
                ..Default::default()
            };
            let (key, surface) = render_target_key(rt_base, rt_size, target);
            let mut c1 = Vec::new();
            let id1 = cache.get_render_target(key, surface, ColorFormat::R8G8B8A8Unorm, &mut c1);
            assert_eq!(
                c1.iter()
                    .filter(|c| matches!(c, BackendCmd::CreateRenderTarget { .. }))
                    .count(),
                1,
                "first use creates the RT once"
            );
            let mut c2 = Vec::new();
            let id2 = cache.get_render_target(key, surface, ColorFormat::R8G8B8A8Unorm, &mut c2);
            assert_eq!(id2, id1, "same RT id on reuse");
            assert!(
                c2.is_empty(),
                "the RT entry is clean — a re-fetch emits nothing (no create, no upload)"
            );
        }

        // Restore the env for any following test in this binary.
        unsafe { std::env::remove_var("UNEMUPS4_RT_READBACK") };
    }

    #[test]
    fn deferred_producer_draw_does_not_register_or_emit_its_offscreen_rt() {
        // Regression (commit-before-defer): a draw whose derived target is an OFFSCREEN RT must
        // NOT commit that RT — neither into the submit-spanning `render_targets` registry nor as
        // a CreateRenderTarget/SetRenderTarget in the command stream — until the draw has cleared
        // every clean-defer check. Here the producer's PS samples a 2D macro-tiled texture (no
        // detiler), so the draw defers cleanly AFTER the offscreen target is derived. Before the
        // fix the RT was registered up-front, so a later draw sampling the same range would bind
        // an empty / never-rendered host RT.
        use ps4_core::gpu::registered_display_buffers;
        use ps4_core::gpu::{DisplayBuffer, DisplayBufferSource};

        // A registered videoout framebuffer at a distinct base so the RT base is unambiguously
        // the offscreen target (same disambiguation the sibling RT tests use).
        let fb_base = 0xC000_0000u64;
        struct OneBuffer(DisplayBuffer);
        impl DisplayBufferSource for OneBuffer {
            fn lookup(&self, base: u64) -> Option<DisplayBuffer> {
                (self.0.base == base).then_some(self.0)
            }
        }
        let dbsrc: Arc<dyn DisplayBufferSource> = Arc::new(OneBuffer(DisplayBuffer {
            base: fb_base,
            width: 1920,
            height: 1080,
        }));
        let _dbguard = registered_display_buffers().override_scoped(dbsrc);

        // Offscreen RT at an unregistered 256-aligned base. 8×8 RGBA8 = 256 bytes.
        let rt_base = 0xD000_0000u64;
        let rt_pitch = 8u32;
        let rt_height = 8u32;
        let rt_size = (rt_pitch * rt_height * 4) as u64;

        // The sampled texture lives at a base DISJOINT from the RT range so it resolves as a
        // plain sampled texture (not an RT-as-texture hit) and reaches the macro-tile guard.
        let tex_base = 0xE000_0000u64;

        // Arena: passthrough VS (SSBO fetch) + texture_sample PS + a vec4 vertex buffer + its
        // V# desc set.
        let vs_blob = corpus_sb("passthrough_vs");
        let tex_ps_blob = corpus_sb("texture_sample_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x4000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (tex_ps_addr, cur) = place_aligned(&mut arena, base, cur, &tex_ps_blob);
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        let vs_desc_off = arena.len();
        let vs_desc_ptr = base + vs_desc_off as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // A macro-tiled (2D bank/pipe) sampled T#: tiling_index = 13 at word3[24:20], which
        // `ps4_core::tiling::tile_kind` classifies as Macro2d (no detiler → clean defer).
        let mut tsharp = linear_tsharp(tex_base, 4, 4);
        tsharp[12..16].copy_from_slice(&(13u32 << 20).to_le_bytes());
        let ssharp = point_ssharp();

        // DCB: bind shaders + VS desc set + inline PS T#/S# + offscreen RT regs + one draw.
        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, tex_ps_addr);
        bind_vs_desc_set(&mut dcb, vs_desc_ptr);
        bind_ps_inline_texture(&mut dcb, &tsharp, &ssharp);
        set_offscreen_rt_regs(&mut dcb, rt_base, rt_pitch, rt_height);
        dcb.extend_from_slice(&draw_auto(3));

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        {
            let mut exec = Executor::new(
                ExecMode::Draw,
                &sink,
                &mut state,
                &chain,
                &mut pipelines,
                &mut resources,
            );
            unsafe { exec.run(&range_over(&dcb, false)) };
        }

        // The producer deferred (macro-tiled sampled texture), so no draw was recorded.
        let all: Vec<BackendCmd> = sink.command_lists().into_iter().flatten().collect();
        assert!(
            !all.iter().any(|c| matches!(c, BackendCmd::DrawAuto { .. })),
            "the producer draw deferred cleanly — no DrawAuto is emitted"
        );

        // The crux (commit-before-defer fix): the offscreen range is NOT in the submit-spanning
        // registry, so a later draw sampling [rt_base, rt_base+rt_size) does not falsely resolve
        // to a never-rendered RT and bind an empty/undefined host image.
        assert!(
            state.render_targets.lookup(rt_base, rt_size).is_none(),
            "a deferred producer draw must not register its offscreen RT"
        );
    }

    /// task-187: the DIAGNOSTIC render-target dump is a second, independent path.
    ///
    /// The point of the task is that these two must not be one path with a flag. This drives
    /// the executor with the guest-memory readback OFF and a snapshot capture armed with RT
    /// dumping ON, and asserts the command stream carries exactly the diagnostic command and
    /// none of the readback one — i.e. the diagnostic does not go through, or depend on, the
    /// machinery that refuses macro-tiled surfaces.
    ///
    /// What it does NOT cover: the copy. Turning `DumpRenderTargetPng` into a PNG needs a
    /// live Vulkan device (`AshBackend::dump_render_target_png`), which this crate's
    /// pure-function unit tests cannot create. That link is exercised only by running the
    /// emulator.
    #[test]
    fn armed_snapshot_emits_an_rt_png_dump_independent_of_the_guest_memory_readback() {
        use ps4_core::gpu::registered_display_buffers;
        use ps4_core::gpu::{DisplayBuffer, DisplayBufferSource};

        // Same process-global env vars as the snapshot unit tests, so take their lock.
        let _env = crate::snapshot::tests::DUMP_ROOT_ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("unemups4-exec-rtdump-{}", std::process::id()));
        // SAFETY-of-behaviour: these only steer THIS test; every var is read at use. The
        // UNEMUPS4_RT_READBACK lever is left to the `registered_source`-guarded block below so it
        // serializes with the sibling `rt_readback_policy_*` test (see there).
        unsafe { std::env::set_var(ps4_core::snapshot::DIR_ENV, &tmp) };
        unsafe { std::env::set_var(ps4_core::snapshot::RENDER_TARGETS_ENV, "1") };
        ps4_core::snapshot::clear();

        let fb_base = 0xC000_0000u64;
        struct OneBuffer(DisplayBuffer);
        impl DisplayBufferSource for OneBuffer {
            fn lookup(&self, base: u64) -> Option<DisplayBuffer> {
                (self.0.base == base).then_some(self.0)
            }
        }
        let dbsrc: Arc<dyn DisplayBufferSource> = Arc::new(OneBuffer(DisplayBuffer {
            base: fb_base,
            width: 1920,
            height: 1080,
        }));
        let _dbguard = registered_display_buffers().override_scoped(dbsrc);

        let rt_base = 0xD000_0000u64;
        let (rt_pitch, rt_height) = (8u32, 8u32);

        let vs_blob = corpus_sb("passthrough_vs");
        let flat_ps_blob = corpus_sb("flat_color_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x4000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (flat_ps_addr, cur) = place_aligned(&mut arena, base, cur, &flat_ps_blob);
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        let vs_desc_ptr = base + arena.len() as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, flat_ps_addr);
        bind_vs_desc_set(&mut dcb, vs_desc_ptr);
        set_offscreen_rt_regs(&mut dcb, rt_base, rt_pitch, rt_height);
        dcb.extend_from_slice(&draw_auto(3));

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        // Force the RT-readback lever Off for this run. This mutation is done HERE, under the
        // `registered_source` override guard, on purpose: the only other test that touches
        // UNEMUPS4_RT_READBACK (`rt_readback_policy_*`) also holds that same guard across every
        // read/write of the var, so both tests serialize on it and the reads inside `exec.run`
        // below never race that test's writes. (Mutating it before acquiring the guard — as this
        // test used to — races that test, since it holds the guard while it sets the var.)
        unsafe { std::env::remove_var("UNEMUPS4_RT_READBACK") };

        let sink = MockSink::default();
        let mut state = GpuState::default();
        // Arm through the real cross-thread path (F10 → one frame), not by reaching into the
        // recorder: the arming rule is part of what is under test.
        ps4_core::snapshot::request(1);
        crate::snapshot::on_frame_boundary(&mut state, 42);
        assert!(
            state.snapshot.armed(),
            "a pending request arms the next frame"
        );

        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };
        let cmds = sink.command_lists().into_iter().next().expect("one submit");

        // The readback lever is OFF, so its command must be absent — and the dump is present
        // anyway. That is the separation: the diagnostic does not travel on the readback.
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, BackendCmd::ReadbackRenderTarget { .. })),
            "the guest-memory readback stays off; the diagnostic must not turn it on"
        );
        let dumps: Vec<(ResourceId, std::path::PathBuf)> = cmds
            .iter()
            .filter_map(|c| match c {
                BackendCmd::DumpRenderTargetPng { id, path } => Some((*id, path.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(dumps.len(), 1, "exactly one dump for the one offscreen RT");
        let created: Vec<ResourceId> = cmds
            .iter()
            .filter_map(|c| match c {
                BackendCmd::CreateRenderTarget { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(
            dumps[0].0, created[0],
            "the dump names that RT's ResourceId"
        );
        assert_eq!(
            dumps[0].1,
            tmp.join("frame-00042")
                .join("render-targets")
                .join(format!("rt-{rt_base:016x}-{rt_pitch}x{rt_height}.png")),
            "the PNG lands under the captured frame's directory"
        );
        // It must follow the producer draw: the copy has to read a rendered target.
        let draw_pos = cmds
            .iter()
            .position(|c| matches!(c, BackendCmd::DrawAuto { .. }))
            .expect("a producer draw was emitted");
        let dump_pos = cmds
            .iter()
            .position(|c| matches!(c, BackendCmd::DumpRenderTargetPng { .. }))
            .expect("a dump was emitted");
        assert!(dump_pos > draw_pos, "the dump follows the draw into the RT");

        // And the draw's own record points at that file, so no reader has to guess.
        crate::snapshot::on_frame_boundary(&mut state, 43);
        crate::snapshot::flush_writes();
        let draws = std::fs::read_to_string(tmp.join("frame-00042").join("draws.json"))
            .expect("the armed frame wrote draws.json");
        assert!(
            draws.contains(&format!(
                "render-targets/rt-{rt_base:016x}-{rt_pitch}x{rt_height}.png"
            )),
            "draws.json must reference the dumped target: {draws}"
        );

        unsafe { std::env::remove_var(ps4_core::snapshot::RENDER_TARGETS_ENV) };
        unsafe { std::env::remove_var(ps4_core::snapshot::DIR_ENV) };
        ps4_core::snapshot::clear();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn gcn_draw_index_2_end_to_end_emits_indexbuffer_and_drawindexed() {
        // AC #2: same real-VS setup, but the draw is DRAW_INDEX_2 with a 16-bit index
        // buffer preceded by IT_INDEX_TYPE(16). The index buffer is pulled through the
        // resource cache (create+upload) and a DrawIndexed is emitted. All expectations
        // are independently-reasoned literals.
        let vs_blob = corpus_sb("passthrough_vs");
        let ps_blob = corpus_sb("flat_color_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x2000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (ps_addr, cur) = place_aligned(&mut arena, base, cur, &ps_blob);
        // Vertex data: 3 vec4-float vertices (48 bytes).
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        // Descriptor set: one vec4 V#.
        let desc_off = arena.len();
        let desc_ptr = base + desc_off as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        // Index buffer: three 16-bit indices [0,1,2] → 6 bytes.
        let idx_off = arena.len();
        let idx_addr = base + idx_off as u64;
        arena.extend_from_slice(&0u16.to_le_bytes());
        arena.extend_from_slice(&1u16.to_le_bytes());
        arena.extend_from_slice(&2u16.to_le_bytes());
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // DCB: shader binds + VS desc set + INDEX_TYPE(16-bit) + DRAW_INDEX_2 carrying the
        // index base + count. DRAW_INDEX_2 body = [max_size, base_lo, base_hi, count, init].
        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, ps_addr);
        bind_vs_desc_set(&mut dcb, desc_ptr);
        // IT_INDEX_TYPE body [vgt_index_type]: 0 = 16-bit.
        dcb.push(t3_header(op::IT_INDEX_TYPE, 1));
        dcb.push(0);
        // IT_DRAW_INDEX_2 body [max_size, base_lo, base_hi, index_count, draw_initiator].
        dcb.push(t3_header(op::IT_DRAW_INDEX_2, 5));
        dcb.push(3); // max_size (VGT_DMA_MAX_SIZE)
        dcb.push((idx_addr & 0xFFFF_FFFF) as u32);
        dcb.push((idx_addr >> 32) as u32);
        dcb.push(3); // index_count
        dcb.push(0); // draw_initiator

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        // Expected list (independently reasoned): the shared setup (create + bind +
        // vertex-data create/upload + storage-bind + viewport + scissor) then the INDEX
        // buffer pulled through the cache (create res id 2 of 6 bytes + upload) and
        // DrawIndexed. The passthrough_vs fetches through an SSBO, so its vertex data binds
        // as a storage buffer, not vertex-input:
        //   [0] CreatePipeline{id 1}      [1] BindPipeline{id 1}
        //   [2] CreateBuffer{res 1, 48}   [3] UploadBuffer{res 1, 48}
        //   [4] BindStorageBuffer{set 0, binding 0, res 1, num_records 3}
        //   [5] SetViewport               [6] SetScissor
        //   [7] CreateBuffer{res 2, 6}    [8] UploadBuffer{res 2, 6}
        //   [9] DrawIndexed{res 2, count 3, U16}
        let lists = sink.command_lists();
        assert_eq!(lists.len(), 1);
        let cmds = &lists[0];
        assert_eq!(cmds.len(), 10, "indexed real-VS draw is a 10-command list");
        assert!(
            matches!(cmds[0], BackendCmd::CreatePipeline { .. }),
            "cmds[0] must be CreatePipeline, got {:?}",
            cmds[0]
        );
        assert_eq!(cmds[1], BackendCmd::BindPipeline { id: PipelineId(1) });
        assert_eq!(
            cmds[2],
            BackendCmd::CreateBuffer {
                id: ResourceId(1),
                size: 48
            },
            "vertex data is res id 1"
        );
        assert!(matches!(
            cmds[3],
            BackendCmd::UploadBuffer {
                id: ResourceId(1),
                ..
            }
        ));
        assert_eq!(
            cmds[4],
            BackendCmd::BindStorageBuffer {
                set: 0,
                binding: 0,
                id: ResourceId(1),
                num_records: 3,
                stride: 16,
                // vec4_vsharp encodes the identity swizzle [4,5,6,7] → word3[11:0] = 0xFAC.
                dst_sel: 0xFAC,
                // vec4_vsharp is dfmt 14 (_32_32_32_32) / nfmt 7 (float) → packed 0x70E; the
                // recompiled VS takes the raw-dword path for it (task-164).
                format: 0x70E,
                // Single stream → its push-constant group is at offset 0.
                pc_offset: 0,
            }
        );
        assert!(matches!(cmds[5], BackendCmd::SetViewport(_)));
        assert!(matches!(cmds[6], BackendCmd::SetScissor(_)));
        assert_eq!(
            cmds[7],
            BackendCmd::CreateBuffer {
                id: ResourceId(2),
                size: 6
            },
            "index buffer is the 2nd cached resource (id 2), 3 * 2 bytes"
        );
        match &cmds[8] {
            BackendCmd::UploadBuffer { id, offset, data } => {
                assert_eq!(*id, ResourceId(2));
                assert_eq!(*offset, 0);
                assert_eq!(data.len(), 6, "three 16-bit indices = 6 bytes");
                assert_eq!(&data[..], &[0, 0, 1, 0, 2, 0], "index bytes [0,1,2] LE");
            }
            other => panic!("cmds[8] must be UploadBuffer, got {other:?}"),
        }
        assert_eq!(
            cmds[9],
            BackendCmd::DrawIndexed {
                id: ResourceId(2),
                index_count: 3,
                index_type: IndexType::U16
            }
        );
    }

    #[test]
    fn gcn_draw_index_offset_pulls_offset_subrange_and_draws_indexed() {
        // The DRAW_INDEX_OFFSET_2 (0x35) arm: the packet carries no index base, so the base
        // comes from a preceding IT_INDEX_BASE and the draw reads `index_count` indices
        // starting `index_offset` elements in. Verify the resource pulled is the OFFSET
        // sub-range (base + offset*elem, count*elem bytes) and a DrawIndexed is emitted.
        let vs_blob = corpus_sb("passthrough_vs");
        let ps_blob = corpus_sb("flat_color_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x2000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (ps_addr, cur) = place_aligned(&mut arena, base, cur, &ps_blob);
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        let desc_off = arena.len();
        let desc_ptr = base + desc_off as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        // Index buffer: five 16-bit indices [9,9,0,1,2]; the draw offsets past the two
        // leading 9s (index_offset = 2) and reads 3 indices [0,1,2].
        let idx_off = arena.len();
        let idx_addr = base + idx_off as u64;
        for v in [9u16, 9, 0, 1, 2] {
            arena.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, ps_addr);
        bind_vs_desc_set(&mut dcb, desc_ptr);
        // IT_INDEX_TYPE(16-bit) then IT_INDEX_BASE(idx_addr) — the bound index state the
        // offset draw reads from.
        dcb.push(t3_header(op::IT_INDEX_TYPE, 1));
        dcb.push(0);
        dcb.push(t3_header(op::IT_INDEX_BASE, 2));
        dcb.push((idx_addr & 0xFFFF_FFFF) as u32);
        dcb.push((idx_addr >> 32) as u32);
        // IT_DRAW_INDEX_OFFSET_2 body [max_size, index_offset, index_count, draw_initiator].
        dcb.push(t3_header(op::IT_DRAW_INDEX_OFFSET_2, 4));
        dcb.push(5); // max_size
        dcb.push(2); // index_offset (skip the two leading 9s)
        dcb.push(3); // index_count
        dcb.push(0); // draw_initiator

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        let lists = sink.command_lists();
        assert_eq!(lists.len(), 1);
        let cmds = &lists[0];
        // The offset sub-range is the 2nd cached resource (id 2), 3 * 2 = 6 bytes, uploaded
        // as the three real indices [0,1,2] — proving the offset skipped the two 9s.
        let upload = cmds
            .iter()
            .find_map(|c| match c {
                BackendCmd::UploadBuffer { id, data, .. } if *id == ResourceId(2) => Some(data),
                _ => None,
            })
            .expect("index sub-range must be uploaded as res id 2");
        assert_eq!(
            &upload[..],
            &[0, 0, 1, 0, 2, 0],
            "offset draw must read indices [0,1,2] past the two leading 9s"
        );
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                BackendCmd::DrawIndexed {
                    id: ResourceId(2),
                    index_count: 3,
                    index_type: IndexType::U16
                }
            )),
            "a DrawIndexed over the offset sub-range must be emitted"
        );
    }

    // ---- task-135: IT_DMA_DATA + IT_INDEX_BUFFER_SIZE + IT_NOP ----

    /// Emit an `IT_DMA_DATA` (0x50) packet, GFX6 body layout
    /// `[engine_control, src_lo, src_hi, dst_lo, dst_hi, command]`. `das_reg` sets the
    /// command word's DAS bit (dst is register space) so a register-variant packet can be
    /// hand-built exactly like Celeste's mem->register stream.
    fn dma_data_packet(src: u64, dst: u64, byte_count: u32, das_reg: bool) -> Vec<u32> {
        let command = (byte_count & 0x1F_FFFF) | if das_reg { 1 << 27 } else { 0 };
        vec![
            t3_header(op::IT_DMA_DATA, 6),
            0x6000_0000, // engine_control (SRC_SEL=3, matching the observed Celeste value)
            (src & 0xFFFF_FFFF) as u32,
            (src >> 32) as u32,
            (dst & 0xFFFF_FFFF) as u32,
            (dst >> 32) as u32,
            command,
        ]
    }

    #[test]
    fn dma_data_memory_to_memory_copies_through_bounded_write_seam() {
        // AC #2: a memory->memory IT_DMA_DATA copies `byte_count` src bytes to dst through
        // the bounded read seam (src) + the SMC-observed write_guest seam (dst) — NOT a raw
        // IdentityMem store. Arena host base == guest base, so IdentityMem (registered as the
        // write seam) writes into the same heap the ArenaReader reads back.
        use ps4_core::write_guest::registered_source as write_registered_source;

        // Arena: a 16-byte src pattern, then a 16-byte zeroed dst.
        let mut arena: Vec<u8> = Vec::with_capacity(0x100);
        arena.resize(0x40, 0);
        let base = arena.as_ptr() as u64;
        let src_off = 0x10usize;
        let dst_off = 0x20usize;
        let pattern: [u8; 16] = *b"DMA_DATA_PAYLOAD";
        arena[src_off..src_off + 16].copy_from_slice(&pattern);
        let src_addr = base + src_off as u64;
        let dst_addr = base + dst_off as u64;
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // Bounded read over the arena; IdentityMem as the SMC-observed write seam (its
        // write_bytes stores to the raw host address, which IS the arena heap here).
        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _rguard = registered_source().override_scoped(reader.clone());
        let writer: Arc<dyn ps4_core::write_guest::WriteGuest> =
            Arc::new(crate::idmem::IdentityMem);
        let _wguard = write_registered_source().override_scoped(writer);

        let body: Vec<u32> = dma_data_packet(src_addr, dst_addr, 16, false)[1..].to_vec();
        unsafe { dispatch_dma_data(&body) };

        // Read the dst back through the same bounded seam: it must now hold the pattern.
        let got = reader.read_ranged(dst_addr, 16).expect("dst readable");
        assert_eq!(
            &got[..],
            &pattern,
            "mem->mem DMA must populate the destination"
        );
    }

    #[test]
    fn dma_data_register_variant_defers_without_writing() {
        // AC #2: a register-destination DMA_DATA (DAS=1, Celeste's variant) is cleanly
        // deferred — no write lands. Register the same reader/writer, aim the packet at a
        // zeroed dst, and assert the dst stays zero.
        use ps4_core::write_guest::registered_source as write_registered_source;

        let mut arena: Vec<u8> = Vec::with_capacity(0x100);
        arena.resize(0x40, 0);
        let base = arena.as_ptr() as u64;
        let src_off = 0x10usize;
        let dst_off = 0x20usize;
        arena[src_off..src_off + 16].copy_from_slice(b"SHOULD_NOT_COPY!");
        let src_addr = base + src_off as u64;
        let dst_addr = base + dst_off as u64;

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _rguard = registered_source().override_scoped(reader.clone());
        let writer: Arc<dyn ps4_core::write_guest::WriteGuest> =
            Arc::new(crate::idmem::IdentityMem);
        let _wguard = write_registered_source().override_scoped(writer);

        let body: Vec<u32> = dma_data_packet(src_addr, dst_addr, 16, true)[1..].to_vec();
        unsafe { dispatch_dma_data(&body) };

        let got = reader.read_ranged(dst_addr, 16).expect("dst readable");
        assert_eq!(
            &got[..],
            &[0u8; 16],
            "a register-space (DAS=1) DMA must NOT write guest memory"
        );
    }

    #[test]
    fn index_buffer_size_clamps_offset_draw_and_dma_nop_are_handled() {
        // AC #1/#3/#4: a full stream with IT_INDEX_BUFFER_SIZE + IT_NOP + a register-variant
        // IT_DMA_DATA + IT_DRAW_INDEX_OFFSET_2 decodes and drives the draw with the index
        // count CLAMPED by IT_INDEX_BUFFER_SIZE — and the NOP / DMA leave no unhandled log and
        // no stray command. The index buffer holds five [0,1,2,3,4]; INDEX_BUFFER_SIZE=3
        // clamps a requested count of 5 down to 3.
        let vs_blob = corpus_sb("passthrough_vs");
        let ps_blob = corpus_sb("flat_color_ps");
        let mut arena: Vec<u8> = Vec::with_capacity(0x2000);
        arena.resize(0x100, 0);
        let base = arena.as_ptr() as u64;
        let (vs_addr, cur) = place_aligned(&mut arena, base, 0x100, &vs_blob);
        let (ps_addr, cur) = place_aligned(&mut arena, base, cur, &ps_blob);
        let vtx_off = (cur + 0xF) & !0xF;
        arena.resize(vtx_off, 0);
        let vtx_addr = base + vtx_off as u64;
        arena.extend_from_slice(&[0u8; 48]);
        let desc_off = arena.len();
        let desc_ptr = base + desc_off as u64;
        arena.extend_from_slice(&vec4_vsharp(vtx_addr, 16, 3));
        // Index buffer: five 16-bit indices [0,1,2,3,4].
        let idx_off = arena.len();
        let idx_addr = base + idx_off as u64;
        for v in [0u16, 1, 2, 3, 4] {
            arena.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, ps_addr);
        bind_vs_desc_set(&mut dcb, desc_ptr);
        dcb.push(t3_header(op::IT_INDEX_TYPE, 1));
        dcb.push(0);
        dcb.push(t3_header(op::IT_INDEX_BASE, 2));
        dcb.push((idx_addr & 0xFFFF_FFFF) as u32);
        dcb.push((idx_addr >> 32) as u32);
        // IT_INDEX_BUFFER_SIZE = 3 (the max index count for the draw below).
        dcb.push(t3_header(op::IT_INDEX_BUFFER_SIZE, 1));
        dcb.push(3);
        // An IT_NOP (guest tag payload) — must be silently skipped.
        dcb.push(t3_header(op::IT_NOP, 1));
        dcb.push(0x6875_000d);
        // A register-variant IT_DMA_DATA (DAS=1) — decoded and cleanly deferred, no command.
        dcb.extend(dma_data_packet(idx_addr, 0x3022c, 0x64, true));
        // IT_DRAW_INDEX_OFFSET_2 requesting 5 indices from offset 0 — clamped to 3.
        dcb.push(t3_header(op::IT_DRAW_INDEX_OFFSET_2, 4));
        dcb.push(5); // packet max_size
        dcb.push(0); // index_offset
        dcb.push(5); // requested index_count (clamped to 3 by INDEX_BUFFER_SIZE)
        dcb.push(0); // draw_initiator

        let reader: Arc<dyn BoundedRead> = Arc::new(ArenaReader {
            raw: base,
            buf: arena,
        });
        let _guard = registered_source().override_scoped(reader);

        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let gcn = crate::shader::gcn::GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, false)) };

        let lists = sink.command_lists();
        assert_eq!(lists.len(), 1, "one submit → one command list");
        let cmds = &lists[0];
        // The index sub-range uploaded is 3 * 2 = 6 bytes (count clamped from 5 to 3 by
        // IT_INDEX_BUFFER_SIZE), holding indices [0,1,2].
        let upload = cmds
            .iter()
            .find_map(|c| match c {
                BackendCmd::UploadBuffer { id, data, .. } if *id == ResourceId(2) => Some(data),
                _ => None,
            })
            .expect("clamped index sub-range must be uploaded as res id 2");
        assert_eq!(
            &upload[..],
            &[0, 0, 1, 0, 2, 0],
            "IT_INDEX_BUFFER_SIZE=3 must clamp the 5-index request down to [0,1,2]"
        );
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                BackendCmd::DrawIndexed {
                    id: ResourceId(2),
                    index_count: 3,
                    index_type: IndexType::U16
                }
            )),
            "the offset draw must emit a DrawIndexed with the clamped count 3"
        );
        // The NOP / register-variant DMA emit NO backend command of their own: the whole
        // list is exactly the offset-draw shape (create+bind+buf+upload+storagebind+vp+sc+
        // idxbuf+idxupload+draw = 10), never a stray copy/upload from the deferred DMA.
        assert_eq!(
            cmds.len(),
            10,
            "NOP + deferred register-DMA add no commands; list is the 10-command offset draw"
        );
    }

    #[test]
    fn clamp_count_at_offset_accounts_for_index_offset() {
        // The offset draw reads `count` indices starting `index_offset` elements in, so the
        // readable bound is `max_size - index_offset`, NOT `max_size`. A buffer of 5 indices
        // (max_size=5) with an offset of 2 leaves only 3 readable; a request of 5 clamps to 3.
        let idx = IndexState {
            max_size: 5,
            ..Default::default()
        };
        // Offset 0 behaves like the plain clamp.
        assert_eq!(idx.clamp_count_at_offset(5, 0), 5);
        assert_eq!(idx.clamp_count_at_offset(10, 0), 5);
        // Offset 2 → only 3 indices remain past the offset.
        assert_eq!(
            idx.clamp_count_at_offset(5, 2),
            3,
            "offset must shrink the readable bound (max_size - index_offset)"
        );
        assert_eq!(
            idx.clamp_count_at_offset(2, 2),
            2,
            "an under-bound count is untouched"
        );
        // Offset at/beyond max_size leaves zero readable indices (saturating).
        assert_eq!(idx.clamp_count_at_offset(5, 5), 0);
        assert_eq!(idx.clamp_count_at_offset(5, 9), 0);

        // Unset max (0) means no clamp at all, regardless of offset.
        let unset = IndexState::default();
        assert_eq!(unset.clamp_count_at_offset(5, 3), 5);
    }

    /// The three BOUNDED `IT_ACQUIRE_MEM` bodies real hardware emits in Celeste's DCBs
    /// (captured PS4 command-stream oracle, frames 0..2111) decode to their 256-byte-unit
    /// byte ranges. `coher_cntl=0x82c40040` is
    /// `CB0_DEST_BASE_ENA | TC_WB_ACTION_ENA | TCL1_ACTION_ENA | TC_ACTION_ENA |
    /// CB_ACTION_ENA` plus the engine-select bit 31 — a colour-buffer → texture barrier,
    /// exactly the GPU-written range the guest-CPU dirty drain cannot observe.
    #[test]
    fn acquire_mem_decodes_bounded_hardware_ranges() {
        // [coher_cntl, size, size_hi, base, base_hi, poll_interval]
        let body = [0x82c4_0040, 0x0000_7f80, 0, 0x02bd_4e00, 0, 0x0a];
        assert_eq!(acquire_mem_range(&body), Some((0x2_bd4e_0000, 0x7f_8000)));

        let body = [0x82c4_0040, 0x0000_2400, 0, 0x02be_8180, 0, 0x0a];
        assert_eq!(acquire_mem_range(&body), Some((0x2_be81_8000, 0x24_0000)));

        let body = [0x82c4_0040, 0x0000_2400, 0, 0x02be_a580, 0, 0x0a];
        assert_eq!(acquire_mem_range(&body), Some((0x2_bea5_8000, 0x24_0000)));
    }

    /// The whole-memory acquire real hardware emits once per frame as the DCB preamble
    /// (`coher_size` saturated) is consumed but names no range to invalidate — honouring it
    /// would re-upload the whole working set every frame for no correctness gain.
    #[test]
    fn acquire_mem_whole_memory_names_no_range() {
        let body = [0x2ec4_7fc0, 0xffff_ffff, 0, 0, 0, 0x0a];
        assert_eq!(acquire_mem_range(&body), None);
    }

    /// Degenerate bodies are skipped, never guessed at: a short body, a zero `coher_cntl`
    /// (no action requested) and a zero coherency size all decode to no range.
    #[test]
    fn acquire_mem_degenerate_bodies_are_skipped() {
        assert_eq!(
            acquire_mem_range(&[0x82c4_0040, 0x2400, 0, 0x02be_8180]),
            None
        );
        assert_eq!(
            acquire_mem_range(&[0, 0x2400, 0, 0x02be_8180, 0, 0x0a]),
            None
        );
        assert_eq!(
            acquire_mem_range(&[0x82c4_0040, 0, 0, 0x02be_8180, 0, 0x0a]),
            None
        );
    }

    /// The high halves are 8 bits wide on GFX7/8 and count 256-byte units, so a full-width
    /// base/size assembles a 48-bit byte address rather than truncating to 32 bits.
    #[test]
    fn acquire_mem_uses_8bit_high_halves() {
        let body = [0x82c4_0040, 0x0000_0001, 0x1ff, 0x0000_0002, 0x2ff, 0x0a];
        // hi is masked to 8 bits: 0x1ff -> 0xff, 0x2ff -> 0xff.
        assert_eq!(
            acquire_mem_range(&body),
            Some((0xff_0000_0002 * 256, 0xff_0000_0001 * 256))
        );
    }

    /// An `IT_ACQUIRE_MEM` packet is CONSUMED by the executor walk — the decoder advances
    /// past its 6 body dwords and the following packet still executes (AC: the parser must
    /// not desync and must stop reporting 0x58 as unhandled).
    #[test]
    fn acquire_mem_packet_is_consumed_by_the_walk() {
        let dcb = [
            t3_header(op::IT_ACQUIRE_MEM, 6),
            0x82c4_0040,
            0x0000_2400,
            0,
            0x02be_8180,
            0,
            0x0a,
            t3_header(op::IT_NOP, 1),
            0,
        ];
        let sink = MockSink::default();
        let mut state = GpuState::default();
        let embedded = EmbeddedShaderProvider::new();
        let providers: [&dyn ShaderProvider; 1] = [&embedded];
        let chain = ChainProvider::new(&providers);
        let mut pipelines = PipelineCache::new();
        let mut resources = ResourceCache::new();
        let mut exec = Executor::new(
            ExecMode::Draw,
            &sink,
            &mut state,
            &chain,
            &mut pipelines,
            &mut resources,
        );
        unsafe { exec.run(&range_over(&dcb, true)) };
        // The walk reached the trailing flip, so it did not desync on the 6-dword body.
        assert_eq!(sink.flips.load(Ordering::SeqCst), 1);
    }

    /// task-201: an RT-as-texture bind must sample with the GUEST's S#, not a fixed default.
    ///
    /// Celeste renders at 320x180 and upscales to 1920x1080; a console capture of that frame
    /// shows the upscale draw's S# selecting NEAREST with CLAMP_LAST_TEXEL on both axes
    /// (and, in the same frame, four bloom draws selecting LINEAR). The RT path used to bind
    /// a hardcoded linear/repeat, so every composite and the final upscale were bilinear —
    /// a blurred frame that no register dump could explain, because the register state was
    /// right and only the bind was wrong.
    #[test]
    fn rt_as_texture_binds_the_guest_s_sharp_not_a_fixed_default() {
        use crate::vbuf::SamplerState;

        // A 320x180 RGBA8 offscreen target, as Celeste's composite chain uses.
        let rt = crate::state::RegisteredRt {
            base: 0x9_afb5_8000,
            size: 320 * 180 * 4,
            desc: TargetDesc {
                width: 320,
                height: 180,
                pitch: 384,
                ..TargetDesc::default()
            },
        };
        let binding = TextureBinding { set: 0, binding: 1 };

        // Helper: run one bind and return (returned desc, the desc of the CreateSampler
        // actually emitted to the backend). Asserting BOTH proves the value the method
        // reports is the value the GPU is told to use.
        let run = |sampler: Option<&SamplerState>| {
            let sink = MockSink::default();
            let mut state = GpuState::default();
            let embedded = EmbeddedShaderProvider::new();
            let providers: [&dyn ShaderProvider; 1] = [&embedded];
            let chain = ChainProvider::new(&providers);
            let mut pipelines = PipelineCache::new();
            let mut resources = ResourceCache::new();
            let mut exec = Executor::new(
                ExecMode::PresentSubset,
                &sink,
                &mut state,
                &chain,
                &mut pipelines,
                &mut resources,
            );
            let mut cmds: Vec<BackendCmd> = Vec::new();
            let returned = exec.bind_render_target_as_texture(binding, &rt, sampler, &mut cmds);
            let created: Vec<SamplerDesc> = cmds
                .iter()
                .filter_map(|c| match c {
                    BackendCmd::CreateSampler { desc, .. } => Some(*desc),
                    _ => None,
                })
                .collect();
            assert_eq!(created.len(), 1, "exactly one CreateSampler per RT bind");
            // And the bind must actually reference a sampler at the right slot.
            assert!(
                cmds.iter().any(|c| matches!(
                    c,
                    BackendCmd::BindTexture {
                        set: 0,
                        binding: 1,
                        ..
                    }
                )),
                "the RT bind must emit its BindTexture"
            );
            (returned, created[0])
        };

        // The real Celeste upscale S#: point filter, CLAMP_LAST_TEXEL on both axes.
        let nearest_clamp = SamplerState {
            bilinear: false,
            clamp_x: SamplerAddressMode::ClampToEdge,
            clamp_y: SamplerAddressMode::ClampToEdge,
        };
        let (returned, created) = run(Some(&nearest_clamp));
        assert_eq!(
            created, returned,
            "reported desc == the desc handed to the backend"
        );
        assert_eq!(
            created.mag_filter,
            SamplerFilter::Nearest,
            "pixel art must not be bilinear"
        );
        assert_eq!(created.min_filter, SamplerFilter::Nearest);
        assert_eq!(created.address_mode_u, SamplerAddressMode::ClampToEdge);
        assert_eq!(created.address_mode_v, SamplerAddressMode::ClampToEdge);

        // The SAME path must still honour a guest that genuinely asks for bilinear — the
        // fix is "use the S#", not "force NEAREST". Celeste's bloom draws depend on this.
        let linear_clamp = SamplerState {
            bilinear: true,
            clamp_x: SamplerAddressMode::ClampToEdge,
            clamp_y: SamplerAddressMode::ClampToEdge,
        };
        let (_, created) = run(Some(&linear_clamp));
        assert_eq!(
            created.mag_filter,
            SamplerFilter::Linear,
            "a LINEAR S# stays LINEAR"
        );
        assert_eq!(created.address_mode_u, SamplerAddressMode::ClampToEdge);

        // Per-axis wrap is carried through independently, not collapsed to one mode.
        let mixed = SamplerState {
            bilinear: false,
            clamp_x: SamplerAddressMode::Repeat,
            clamp_y: SamplerAddressMode::ClampToEdge,
        };
        let (_, created) = run(Some(&mixed));
        assert_eq!(created.address_mode_u, SamplerAddressMode::Repeat);
        assert_eq!(created.address_mode_v, SamplerAddressMode::ClampToEdge);

        // No S# at all → the portable default (linear/repeat) is the fallback, and ONLY
        // then. This is the one case the old hardcoded value was ever correct for.
        let (_, created) = run(None);
        assert_eq!(created.mag_filter, SamplerFilter::Linear);
        assert_eq!(created.min_filter, SamplerFilter::Linear);
        assert_eq!(created.address_mode_u, SamplerAddressMode::Repeat);
        assert_eq!(created.address_mode_v, SamplerAddressMode::Repeat);
    }

    /// The pure decision function behind every bind path, including the no-S# fallback.
    #[test]
    fn sampler_desc_for_maps_filter_and_per_axis_wrap() {
        use crate::vbuf::SamplerState;
        let d = sampler_desc_for(Some(&SamplerState {
            bilinear: false,
            clamp_x: SamplerAddressMode::ClampToEdge,
            clamp_y: SamplerAddressMode::MirrorRepeat,
        }));
        assert_eq!(d.mag_filter, SamplerFilter::Nearest);
        assert_eq!(d.min_filter, SamplerFilter::Nearest);
        assert_eq!(d.address_mode_u, SamplerAddressMode::ClampToEdge);
        assert_eq!(d.address_mode_v, SamplerAddressMode::MirrorRepeat);

        let d = sampler_desc_for(None);
        assert_eq!(d.mag_filter, SamplerFilter::Linear);
        assert_eq!(d.address_mode_u, SamplerAddressMode::Repeat);
    }
}
