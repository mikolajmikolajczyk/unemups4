//! The executor (doc-4 §1, §3): consumes a decoded PM4 stream and acts on it. It
//! is backend-agnostic and Vulkan-free — present crosses the display-thread channel
//! through the `ps4_core::gpu::PresentSink` seam, and GPU→CPU sync (EOP/EOS labels)
//! is a synchronous write into identity-mapped guest memory. Phases 3.5/4 add draw
//! and shader arms here; the loop is written once (doc-4 §3 "one pipeline").
//!
//! Phase 3: `ExecMode::PresentSubset`. Decode+trace still run in every
//! mode; this file adds the *present/sync* arms on top:
//!
//!  * `SubmitAndFlip` (the submit range's flip flag) → drive the existing softgpu
//!    present path via [`PresentSink::submit_and_flip`] (reused).
//!  * `IT_EVENT_WRITE_EOP` / `IT_EVENT_WRITE_EOS` → write the label value to the
//!    guest address the packet names, so a guest that submits then waits on the
//!    EOP label proceeds (doc-4 §C2 timeline model; synchronous for now, no async
//!    GPU thread).
//!
//! No draws, no shaders, no state application yet. Unknown/unhandled
//! opcodes are decoded and skipped, never fatal.

use ps4_core::bounded_read::bounded_read;
use ps4_core::dirty::{AlwaysDirty, DirtySource, dirty_source};
use ps4_core::gpu::{
    BackendCmd, IndexType, MAX_VERTEX_ATTRIBUTES, PresentSink, PushConstantRange,
    SamplerAddressMode, SamplerDesc, SamplerFilter, ScissorRect, StorageBinding, TargetDesc,
    TextureBinding, TextureFormat, VertexAttr, VertexBinding, VertexFormat, VertexLayout,
    ViewportRect,
};
use ps4_core::memory::MemoryAccessExt;

use crate::cache::{Compression, Extent, SurfaceFormat, SurfaceLayout, TexelSize, Tiling};
use crate::cache::{ResLayout, ResourceCache, ResourceKey};
use crate::derive::{Scissor, Viewport};
use crate::driver::SubmitRange;
use crate::idmem::IdentityMem;
use crate::pm4::decode::{self, OwnedPacket};
use crate::pm4::opcodes::op;
use crate::shader::pipeline_cache::{PipelineCache, PipelineLookup};
use crate::shader::sb::{SbParseError, SbShader, parse_sb};
use crate::shader::source::{HostShader, ShaderProvider, Stage};
use crate::state::{BoundShaders, GpuState};
use crate::vbuf::{
    BufferSlot, CORPUS_TEXTURE_SLOT, DrawBuffers, FetchLayout, TextureBindingRange, UserData,
    derive_buffer_ranges, derive_texture,
};

/// Which packet families the executor acts on. Decode+trace run in every mode; the
/// mode gates *execution* so trace-only, present-subset and
/// full draw are three configurations of one loop, not forks (doc-4 §3).
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
/// into later ones (doc-4 §5/§C7), and the executor is created fresh per submit. See
/// the [`driver()`](crate::driver::driver) lock invariant — the executor mutates
/// this state while a present is in flight.
pub struct Executor<'a> {
    mode: ExecMode,
    sink: &'a dyn PresentSink,
    state: &'a mut GpuState,
    /// The single route every draw's VS/PS bind resolves through (doc-4 §4). A
    /// composite ([`ChainProvider`](crate::shader::source::ChainProvider)) built by
    /// the caller: embedded today, with the GCN provider appended later. The executor
    /// only knows the `ShaderProvider` seam, so a new provider is added to the chain
    /// rather than special-cased here.
    providers: &'a dyn ShaderProvider,
    /// Guest-side host-pipeline cache (doc-4 §4, decision-7). Borrowed from the driver
    /// so a pipeline bound in an earlier submit resolves to the same id here — the miss
    /// path (which emits `CreatePipeline`) runs at most once per distinct pipeline.
    pipelines: &'a mut PipelineCache,
    /// Guest-side resource cache (doc-4 §8): the draw pulls its referenced vertex/index
    /// buffers through this (upload-on-use, dirty-invalidate). Driver-owned so a buffer
    /// uploaded in an earlier submit is reused here.
    resources: &'a mut ResourceCache,
    /// The dirty-tracking source the resource cache watches ranges against (doc-4 §8.3).
    /// The registered x86jit-backed source when wired, else an [`AlwaysDirty`] fallback
    /// (re-upload every submit — correct, not incremental). Held for the executor's life
    /// so `ResourceCache::get`'s `watch` calls reach one stable source.
    dirty: std::sync::Arc<dyn DirtySource>,
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
        // submit, which is correct if not incremental (doc-4 §8.3).
        let dirty = dirty_source().unwrap_or_else(|| std::sync::Arc::new(AlwaysDirty::new()));
        Self {
            mode,
            sink,
            state,
            providers,
            pipelines,
            resources,
            dirty,
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
        let draw_on = self.mode == ExecMode::Draw;
        let packets = unsafe { decode::decode_submit_range(range) };
        // ONE command list per submit (doc-4 §3 data-list model): every draw in this
        // submission appends its pipeline/bind/draw commands here, and the whole list
        // ships once after the walk. The display thread records it into ONE command
        // buffer behind the per-frame fence — no per-draw channel round-trip / GPU stall.
        let mut cmds: Vec<BackendCmd> = Vec::new();
        // Index-draw state accumulated across the submit walk (doc-4 §5): the last
        // `IT_INDEX_TYPE` width and the last `IT_INDEX_BASE` address seed a following
        // `IT_DRAW_INDEX_2` (which also carries its own base — that supersedes). Reset
        // per submit; a draw before any index-type packet defaults to 16-bit indices.
        let mut index_state = IndexState::default();
        for pkt in &packets {
            if let OwnedPacket::Type3 { opcode, body, .. } = pkt {
                match *opcode {
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
                        if let Some(base) = crate::pm4::opcodes::set_reg_base(*opcode) {
                            self.state.apply_set_reg(base, body);
                        }
                    }
                    // IT_CLEAR_STATE resets the register banks (CONTEXT_CONTROL /
                    // CLEAR_STATE full-clear, doc-4 §C7). The bound-shader view is
                    // left intact (a separate guest bind, re-emitted per draw).
                    op::IT_CLEAR_STATE => self.state.clear_regs(),
                    // Index-draw state (doc-4 §5): these packets only record state; the
                    // draw arms below consume it. Applied in Draw mode (they gate the
                    // indexed-draw path).
                    op::IT_INDEX_TYPE if draw_on => index_state.set_type(body),
                    op::IT_INDEX_BASE if draw_on => index_state.set_base(body),
                    // NUM_INSTANCES carries the instance count; instancing >1 is deferred
                    // (count only, doc-4 §5) — a >1 count is logged and the draw runs a
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
                    _ => {}
                }
            }
        }
        // Ship the whole submit as one list (nothing to send if no draw resolved).
        if !cmds.is_empty() {
            self.sink.run_command_list(&cmds);
        }
        if range.flip {
            self.sink.submit_and_flip();
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
            cmds.push(BackendCmd::DrawAuto { vertex_count });
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
        let [base_lo, base_hi, index_count] = match body {
            [_max_size, base_lo, base_hi, index_count, ..] => [*base_lo, *base_hi, *index_count],
            _ => {
                tracing::debug!("[GNM] DrawIndex2 body too short; deferring draw");
                return;
            }
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

    /// Shared draw setup for `DrawIndexAuto`/`DrawIndex2` (doc-4 §4/§5/§C4). Resolves the
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
        // precedence per stage (doc-4 §5). A register-programmed shader becomes a
        // `GcnBinary` ref → recompiled here through the provider chain.
        let bound = self.state.derive_bound_shaders();

        let (vs_host, ps_host) = match resolve_shader_pair(self.providers, &bound, &*self.dirty) {
            ShaderPairResolution::Resolved { vs, ps } => (vs, ps),
            ShaderPairResolution::NeedsGcn => {
                // AC #3: recognized-but-unsupported real `.sb` GCN shader. Clean defer,
                // not a crash. debug! so headless oracle baselines (no draw path wired)
                // are untouched.
                tracing::debug!(
                    "[GNM] {what} bound to a non-recompilable (.sb GCN) shader — \
                     deferring draw (count={count})"
                );
                return None;
            }
            ShaderPairResolution::Unbound => {
                tracing::debug!(
                    "[GNM] {what} with no resolvable VS/PS bound — skipping draw (count={count})"
                );
                return None;
            }
        };

        // Derive the render target + pipeline state from the shadow registers (doc-4 §5).
        // A programmed-but-unsupported or unregistered target is a clean defer + log.
        let draw = self.derive_draw_state(&bound, what, count)?;

        // Register-derived vertex ranges (doc-4 §C4): read the VS user-SGPR block, build a
        // fetch layout from the recompiled VS's descriptor bindings, and decode each V#. A
        // bad/null descriptor defers cleanly; an embedded VS has no fetch and yields an
        // empty result.
        let draw_buffers = self.derive_vertex_buffers(&vs_host);

        // A recompiled VS that declares an SSBO buffer binding fetches its vertices through
        // a storage buffer indexed by `gl_VertexIndex` (doc-4 §C4): it consumes no
        // vertex-input, so the host pipeline must be built with a descriptor set + push
        // constant rather than vertex-input attributes. An embedded VS (io: None) keeps the
        // register-derived vertex-input path unchanged.
        let ssbo = vs_host
            .io
            .as_ref()
            .and_then(|io| io.buffers.first().map(|b| (io, b)));

        // Sampled-texture path (doc-4 §C3/§C4): a recompiled PS that declares a combined
        // image-sampler binding (io.samplers non-empty) samples a texture. Resolve the
        // T#/S# from the PS user-SGPR block through the bounded seam BEFORE building the
        // pipeline. IMPORTANT: if the PS needs a texture but the T#/S# can't resolve, the
        // whole draw defers cleanly — building a pipeline with `texture: Some` and then
        // failing to emit a `BindTexture` would leave the combined image-sampler
        // descriptor un-written (a validation error / GPU crash).
        let sampler_binding = ps_host
            .io
            .as_ref()
            .and_then(|io| io.samplers.first().copied());
        let texture_binding = if let Some(sb) = sampler_binding {
            let Some(resolved) = self.derive_texture_binding() else {
                tracing::debug!(
                    "[GNM] {what} PS declares a sampler but the T#/S# did not resolve — \
                     deferring draw (count={count})"
                );
                return None;
            };
            // 2D macro-tiling (bank/pipe swizzle) has no detiler yet: detiling it as 1D
            // would ship silently-wrong texels the oracle can't match. Defer cleanly
            // rather than mis-detile (task-98 AC#2).
            if ps4_core::tiling::tile_kind(resolved.texture.tiling_index)
                == ps4_core::tiling::TileKind::Macro2d
            {
                tracing::debug!(
                    tiling_index = resolved.texture.tiling_index,
                    "[GNM] {what} texture is 2D macro-tiled (no detiler) — deferring draw \
                     (count={count})"
                );
                return None;
            }
            Some((
                TextureBinding {
                    set: sb.set,
                    binding: sb.binding,
                },
                resolved,
            ))
        } else {
            None
        };

        let mut key = draw.pipeline;
        let (storage, push_constants): (Option<StorageBinding>, Option<PushConstantRange>) =
            if let Some((io, binding)) = ssbo {
                // No phantom vertex input for the SSBO path: the VS reads no vertex-input,
                // so an attribute here would be an invalid pipeline the driver faults on.
                key.vertex_layout = None;
                let storage = StorageBinding {
                    set: binding.set,
                    binding: binding.binding,
                    stride: binding.stride_bytes,
                };
                // The `num_records` push constant clamps the VS fetch; a missing value
                // silently clamps every vertex to element 0. Cover the declared fields'
                // extent so the pipeline layout's range matches what the draw pushes.
                let pc = io
                    .push_constants
                    .iter()
                    .map(|f| f.offset_bytes + f.size_bytes)
                    .max()
                    .map(|end| PushConstantRange {
                        offset: 0,
                        size: end,
                    });
                (Some(storage), pc)
            } else {
                // Embedded / vertex-input path: fold the register-derived vertex-input
                // layout into the pipeline key. Ok(Some) = a built layout; Ok(None) = the
                // embedded no-fetch path (empty vertex input); Err = an unbuildable layout
                // → defer the draw cleanly.
                key.vertex_layout = vertex_layout_of(&draw_buffers).ok()?;
                (None, None)
            };

        // Guest-side cache: get-or-mint the id for this key. On a MISS the SPIR-V crosses
        // the channel once (CreatePipeline); on a HIT only the small bind id (decision-7).
        let id = match self.pipelines.get_or_mint(key) {
            PipelineLookup::Miss(id) => {
                cmds.push(BackendCmd::CreatePipeline {
                    id,
                    vs_spirv: vs_host.spirv.clone(),
                    ps_spirv: ps_host.spirv.clone(),
                    key: Box::new(key),
                    target: draw.target,
                    storage,
                    push_constants,
                    // The combined image-sampler binding a sampling PS declares, or None.
                    texture: texture_binding.map(|(b, _)| b),
                });
                id
            }
            PipelineLookup::Hit(id) => id,
        };
        cmds.push(BackendCmd::BindPipeline { id });

        // Sampled texture: pull the guest texture through the cache (detile → upload) and
        // create the sampler, then bind the combined image-sampler. The T#/S# already
        // resolved (or the draw deferred above), so a `texture: Some` pipeline always gets
        // its matching `BindTexture` — the descriptor is never left un-written.
        if let Some((binding, resolved)) = texture_binding {
            self.bind_texture(binding, &resolved, cmds);
        }

        if let Some((_, binding)) = ssbo {
            // The passthrough VS fetches from exactly one SSBO range. Pull its bytes through
            // the resource cache (upload-on-use) and bind it as a storage buffer, supplying
            // the V#'s `num_records` as the fetch clamp.
            let Some(range) = draw_buffers
                .ranges
                .iter()
                .find(|r| r.layout == ResLayout::VertexBuf)
            else {
                tracing::debug!(
                    "[GNM] {what} SSBO VS resolved no vertex range — deferring draw (count={count})"
                );
                return None;
            };
            // The recompiler bakes a fixed 16-byte element stride into the module (it cannot
            // see the guest V# stride symbolically), so a bound V# whose stride differs
            // would mis-address the fetch. Stride 0 is a tightly-packed vec4 (matches 16).
            if range.desc.stride != 0 && range.desc.stride != binding.stride_bytes {
                tracing::debug!(
                    "[GNM] {what} SSBO V# stride {} != baked stride {} — deferring draw \
                     (count={count})",
                    range.desc.stride,
                    binding.stride_bytes
                );
                return None;
            }
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
            });
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

        // Dynamic viewport/scissor (doc-4 §5): the pipeline declares the dynamic states,
        // so the register-derived rects cross as plain data here.
        cmds.push(BackendCmd::SetViewport(viewport_rect(draw.viewport)));
        cmds.push(BackendCmd::SetScissor(scissor_rect(draw.scissor)));
        let _ = id;
        Some(())
    }

    /// Derive the register-derived vertex buffers a draw references (doc-4 §C4). Builds a
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

    /// Resolve the PS's sampled-texture T#/S# through the bounded seam (doc-4 §C4). The
    /// T#/S# pointers are register-derived and untrusted, so the read is range-validated.
    /// Returns `None` (a clean defer) when the seam is unwired, the pointer is null, the
    /// read faults, or the T# is degenerate — the caller then defers the whole draw so a
    /// `texture: Some` pipeline never lacks its `BindTexture`.
    fn derive_texture_binding(&self) -> Option<TextureBindingRange> {
        let user = UserData::from_regs(self.state, Stage::Pixel);
        let reader = bounded_read()?;
        match derive_texture(&user, &CORPUS_TEXTURE_SLOT, reader.as_ref()) {
            Ok(range) => Some(range),
            Err(e) => {
                tracing::debug!("[GNM] PS texture T#/S# did not resolve: {e:?}");
                None
            }
        }
    }

    /// Pull the resolved texture through the resource cache (detile → CreateImage +
    /// UploadImage on first use / after a guest write; nothing on a clean hit) and get-or-
    /// create its sampler, then bind the combined image-sampler at `binding` (doc-4 §C3/
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
        // Map the T#'s tiling index to the detile mode, carrying the raw dfmt/nfmt on the
        // key so two views of the same bytes key apart. A macro-tiled (2D) index has no
        // detiler and is filtered out in `setup_draw` before the pipeline is built, so it
        // cannot reach here — classify defensively rather than silently mis-detiling it.
        let tiling = match ps4_core::tiling::tile_kind(t.tiling_index) {
            ps4_core::tiling::TileKind::Linear => Tiling::LinearGeneral,
            ps4_core::tiling::TileKind::Thin1d => Tiling::Thin1d,
            ps4_core::tiling::TileKind::Macro2d => {
                tracing::error!(
                    tiling_index = t.tiling_index,
                    "bind_texture reached a macro-tiled T# that setup_draw should have \
                     deferred; treating as linear"
                );
                Tiling::LinearGeneral
            }
        };
        let surface = SurfaceLayout {
            texel: TexelSize::Bpp32,
            extent: Extent {
                width: t.width,
                height: t.height,
            },
            tiling,
            compression: Compression::Off,
        };
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
        let image_id = self.resources.get_texture(
            key,
            surface,
            TextureFormat::R8G8B8A8Unorm,
            &crate::idmem::BoundedMem,
            self.dirty.as_ref(),
            cmds,
        );
        // Sampler: fixed portable defaults, with the filter the S# selected (decision-3 —
        // no anisotropy/mips). Repeat addressing is the subset default.
        let filter = if resolved.sampler.bilinear {
            SamplerFilter::Linear
        } else {
            SamplerFilter::Nearest
        };
        let sampler_id = self.resources.get_sampler(
            SamplerDesc {
                mag_filter: filter,
                min_filter: filter,
                address_mode: SamplerAddressMode::Repeat,
            },
            cmds,
        );
        cmds.push(BackendCmd::BindTexture {
            set: binding.set,
            binding: binding.binding,
            image_id,
            sampler_id,
        });
    }

    /// Derive the draw's [`DrawState`] (target + pipeline + viewport + scissor, doc-4 §5),
    /// returning `None` (a clean defer) for an unsupported/unregistered target. A draw
    /// with no color base is the embedded fullscreen corpus: it renders into the videoout
    /// target, so the default `TargetDesc` and a pipeline key over the default format are
    /// used.
    fn derive_draw_state(
        &self,
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
                None
            }
            Err(TargetError::UnregisteredTarget { base }) => {
                tracing::debug!(
                    "[GNM] {what} color base {base:#x} is not a registered display \
                     buffer (arbitrary RT out of scope); deferring draw (count={count})"
                );
                None
            }
        }
    }
}

/// Index-draw state accumulated across a submit walk (doc-4 §5): the element width from
/// `IT_INDEX_TYPE`, the base from `IT_INDEX_BASE`, and the instance count from
/// `IT_NUM_INSTANCES`. A following `IT_DRAW_INDEX_2` consumes it (its own carried base
/// supersedes `base`). Defaults to 16-bit indices, a null base, and one instance.
#[derive(Clone, Copy, Debug, Default)]
struct IndexState {
    index_type: IndexType,
    base: u64,
    instances: u32,
}

impl IndexState {
    /// `IT_INDEX_TYPE` body `[vgt_index_type]`: bits [1:0] select 0=16-bit, 1=32-bit.
    fn set_type(&mut self, body: &[u32]) {
        self.index_type = match body.first().copied().unwrap_or(0) & 0x3 {
            1 => IndexType::U32,
            _ => IndexType::U16,
        };
    }

    /// `IT_INDEX_BASE` body `[addr_lo, addr_hi]`: the 64-bit index buffer base.
    fn set_base(&mut self, body: &[u32]) {
        let lo = body.first().copied().unwrap_or(0);
        let hi = body.get(1).copied().unwrap_or(0);
        self.base = u64::from(lo) | (u64::from(hi) << 32);
    }

    /// `IT_NUM_INSTANCES` body `[instance_count]`. Instancing >1 is deferred (count only,
    /// doc-4 §5): a >1 count is logged; the draw still runs a single instance.
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

/// The recompiled VS's descriptor bindings as a [`FetchLayout`] (doc-4 §C4), or `None`
/// for an embedded VS (no `io`). Each `BufferBinding` becomes a [`BufferSlot`] over the
/// GCN descriptor-set-pointer ABI (`s[2:3]` holds the descriptor-set pointer); the
/// binding index picks the V# within the set. Only the vertex-buffer bindings the VS
/// declares are emitted, keyed as [`ResLayout::VertexBuf`].
fn fetch_layout_of(vs_host: &HostShader) -> Option<FetchLayout> {
    let io = vs_host.io.as_ref()?;
    // The descriptor-set pointer the driver preloads into user SGPRs `s[2:3]` (corpus
    // ABI). Each binding's V# sits at `binding * V_SHARP_SIZE` within that set.
    const DESC_SET_USER_SGPR: usize = 2;
    let buffers = io
        .buffers
        .iter()
        .map(|b| BufferSlot {
            user_sgpr: DESC_SET_USER_SGPR,
            desc_offset: u64::from(b.binding) * crate::vbuf::V_SHARP_SIZE as u64,
            layout: ResLayout::VertexBuf,
        })
        .collect();
    Some(FetchLayout { buffers })
}

/// The vertex-input part of the pipeline key over a draw's decoded vertex buffers
/// (doc-4 §C4). Turns each vertex-buffer V# into one declared attribute + one binding,
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
    /// At least one stage is a recognized-but-unsupported ref (a `.sb` GCN binary
    /// before its translator lands) → defer to phase 4 (AC #3).
    NeedsGcn,
    /// A stage is unbound, or resolved to nothing the chain handles → skip.
    Unbound,
}

/// Resolve the bound VS/PS pair through the injected [`ShaderProvider`] chain
/// (doc-4 §4), carrying the resolved [`HostShader`]s (with their `Arc<[u32]>` SPIR-V)
/// out so the caller can hand the SPIR-V straight onto a `CreatePipeline` command.
///
/// Returns [`ShaderPairResolution::NeedsGcn`] the moment either stage is a
/// recognized-but-unsupported ref (the chain's `Err`, e.g. a `GcnBinary` before its
/// translator lands), so a mixed embedded+GCN bind still defers cleanly. Each stage is
/// resolved **exactly once** so a side-effecting provider (the phase-4 GCN one —
/// `parse_sb` + SPIR-V recompile) never runs twice per draw.
///
/// `dirty` is threaded to the provider so a GCN `.sb` recompile `watch`es its code range
/// at resolve time (doc-4 §8.3) — the resource cache's watch-on-insert shape, for shaders.
/// A later guest write to a watched range invalidates the recompile on the next per-submit
/// [`GcnShaderProvider::drain_dirty`], so a self-modified / reloaded shader re-recompiles.
fn resolve_shader_pair(
    provider: &dyn ShaderProvider,
    bound: &BoundShaders,
    dirty: &dyn DirtySource,
) -> ShaderPairResolution {
    let mem = IdentityMem;
    let vs = bound
        .vs
        .as_ref()
        .map(|r| provider.resolve(r, &mem, Some(dirty)));
    let ps = bound
        .ps
        .as_ref()
        .map(|r| provider.resolve(r, &mem, Some(dirty)));

    // Any recognized-but-unsupported ref in the pair → phase-4 defer. Checked before
    // requiring both bound so a real-shader bind is reported as NeedsGcn, not Unbound.
    if matches!(vs, Some(Err(_))) || matches!(ps, Some(Err(_))) {
        return ShaderPairResolution::NeedsGcn;
    }

    // Both stages resolved to a host shader → draw. A missing bind or an `Ok(None)`
    // (no provider handled the ref) leaves the draw unbound rather than dispatching.
    match (vs, ps) {
        (Some(Ok(Some(vs))), Some(Ok(Some(ps)))) => ShaderPairResolution::Resolved {
            vs: Box::new(vs),
            ps: Box::new(ps),
        },
        _ => ShaderPairResolution::Unbound,
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

/// The 64-bit destination address an EOP/EOS packet writes its label to, assembled
/// from the low dword and the low 16 bits of the address-hi dword (GFX6 layout).
fn label_addr(addr_lo: u32, addr_hi_word: u32) -> u64 {
    (addr_lo as u64) | (((addr_hi_word & 0xFFFF) as u64) << 32)
}

/// Write a value to an identity-mapped guest label address (guest ptr == host ptr,
/// doc-2 §1) — the CPU-visible mirror of the GPU timeline (doc-4 §C2). A zero
/// address is ignored (an EOP that only signals an interrupt names no memory).
///
/// # Safety
/// `addr` must be a writable guest label location; the identity-mapped guest range
/// lives for the whole run, matching how the decoder reads command buffers.
unsafe fn write_label(addr: u64, value: u64) {
    // Identity-mapped store (guest ptr == host ptr); a zero address is unbacked and
    // ignored by the trait, so the EOP-only-interrupt case is a no-op.
    let _ = IdentityMem.write::<u64>(addr, value);
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
    unsafe { write_label(addr, value) };
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
    unsafe { write_label(addr, data as u64) };
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
        cmds: Mutex<Vec<Vec<BackendCmd>>>,
    }
    impl PresentSink for MockSink {
        fn submit_and_flip(&self) {
            self.flips.fetch_add(1, Ordering::SeqCst);
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
    fn eop_writes_64bit_label_to_guest_address() {
        // The label the guest waits on lives in a host buffer; its address IS the
        // guest GPU VA (identity-mapped). EOP must write (data_hi:data_lo) there.
        let mut label: u64 = 0;
        let label_addr = &mut label as *mut u64 as u64;
        let addr_lo = (label_addr & 0xFFFF_FFFF) as u32;
        let addr_hi = ((label_addr >> 32) & 0xFFFF) as u32;

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
        unsafe { exec.run(&range_over(&dcb, false)) };
        assert_eq!(label, 0x0000_0001_CAFE_F00D);
    }

    #[test]
    fn eos_writes_32bit_label_to_guest_address() {
        let mut label: u64 = 0xDEAD_DEAD_DEAD_DEAD;
        let label_addr = &mut label as *mut u64 as u64;
        let addr_lo = (label_addr & 0xFFFF_FFFF) as u32;
        let addr_hi = ((label_addr >> 32) & 0xFFFF) as u32;

        let dcb = [
            t3_header(op::IT_EVENT_WRITE_EOS, 4),
            0x0000_0005,
            addr_lo,
            addr_hi,
            0x1234_5678, // data (32-bit)
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
        // Low 32 bits are the written label; the high half is left as the 64-bit
        // store's zero-extension of the 32-bit datum.
        assert_eq!(label, 0x0000_0000_1234_5678);
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
    use ps4_core::gpu::{IndexType, ResourceId};
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
                storage,
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
                // num_records push-constant range (offset 0, size 4). Hand-reasoned.
                assert_eq!(
                    *storage,
                    Some(StorageBinding {
                        set: 0,
                        binding: 0,
                        stride: 16
                    }),
                    "vec4 SSBO binding: set 0, binding 0, 16-byte stride"
                );
                assert_eq!(
                    *push_constants,
                    Some(PushConstantRange { offset: 0, size: 4 }),
                    "num_records push constant: offset 0, size 4"
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
                num_records: 3
            }
        );
        assert!(matches!(cmds[5], BackendCmd::SetViewport(_)));
        assert!(matches!(cmds[6], BackendCmd::SetScissor(_)));
        assert_eq!(cmds[7], BackendCmd::DrawAuto { vertex_count: 3 });
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

    /// Program the PS user-SGPR pair `s[0:1]` to the PS descriptor-set pointer (corpus
    /// texture ABI: T# at offset 0, S# at offset 32).
    fn bind_ps_desc_set(dcb: &mut Vec<u32>, desc_ptr: u64) {
        let sh = |abs: u32| abs - reg_base_sh();
        dcb.extend(set_reg(
            op::IT_SET_SH_REG,
            sh(sh_reg::SPI_SHADER_USER_DATA_PS_0),
            &[(desc_ptr & 0xFFFF_FFFF) as u32, (desc_ptr >> 32) as u32],
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
        let (tex_addr, cur) = place_aligned(&mut arena, base, tex_cursor, &[0u8; 16]);
        // PS descriptor set (256-aligned so its own address is clean): T# then S#.
        let mut ps_desc = Vec::new();
        ps_desc.extend_from_slice(&linear_tsharp(tex_addr, 2, 2));
        ps_desc.extend_from_slice(&point_ssharp());
        let (ps_desc_ptr, _cur) = place_aligned(&mut arena, base, cur, &ps_desc);
        assert_eq!(arena.as_ptr() as u64, base, "arena must not reallocate");

        // DCB: shader binds + VS desc-set + PS desc-set + DRAW_INDEX_AUTO.
        let mut dcb: Vec<u32> = Vec::new();
        bind_gcn_shaders(&mut dcb, vs_addr, ps_addr);
        bind_vs_desc_set(&mut dcb, vs_desc_ptr);
        bind_ps_desc_set(&mut dcb, ps_desc_ptr);
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

        // The pipeline must declare a combined image-sampler binding (texture Some).
        let tex_binding = match &cmds[0] {
            BackendCmd::CreatePipeline { texture, .. } => {
                texture.expect("textured pipeline declares a combined image-sampler binding")
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
        // Point S# → Nearest filter, Repeat addressing (the subset defaults). Hand-reasoned.
        assert_eq!(sdesc.mag_filter, SamplerFilter::Nearest);
        assert_eq!(sdesc.min_filter, SamplerFilter::Nearest);
        assert_eq!(sdesc.address_mode, SamplerAddressMode::Repeat);

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
                num_records: 3
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
}
