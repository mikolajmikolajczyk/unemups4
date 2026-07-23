//! ash implementation of the `ps4-core` `GpuBackend` trait (doc-2 §2, §7 step 1).
//!
//! `AshBackend` owns the `VulkanContext` and the display-side GPU bookkeeping
//! (registered buffers, zero-copy imports, the current flip target, the pending
//! guest vsync signal). `present()` runs the exact softgpu present chain that used
//! to be open-coded in `run_display_loop`: fence wait → acquire → fb copy/import
//! → record → submit → queue present, with the vsync-signal timing
//! preserved byte-for-byte. All `ash`/`vk::*` stays behind this leaf.

use ash::vk;
use crossbeam_channel::Sender;
use ps4_core::gpu::{
    BackendCmd, BlendKey, ColorFormat, GpuBackend, GpuError, IndexType, PipelineId, PipelineKey,
    PrimitiveTopology as CorePrimitiveTopology, PushConstantRange, ResourceDesc, ResourceId,
    SamplerDesc, ScissorRect, StorageBinding, TargetDesc, TargetId, TextureBinding, TextureFormat,
    ViewportRect,
};
use ps4_core::memory::VirtualMemoryManager;
// task-56 step 5: the RT readback re-tiles its linear pixels to the guest surface layout,
// reusing the SAME `tile` inverse the upload path detiles with (so the two never drift).
use ps4_gnm::cache::{Compression, Extent, SurfaceLayout, TexelSize, Tiling};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::commands::DisplayBuffer;
use crate::present_profile::{self, PRESENT, SUBMIT};
use crate::vulkan::{ImportedBuf, VulkanContext};

const RES_W: u32 = 1920;
const RES_H: u32 = 1080;

/// Decode whether the guest scanout `pixelFormat` stores channels in BGRA byte order,
/// which the present shader must R↔B-swap when sampling into the RGBA swapchain (task-154
/// residual #2). Only two formats appear in the corpus:
///
/// - `0x8000_0000` = A8R8G8B8_SRGB (Celeste — BGRA byte order → swap).
/// - the A8B8G8R8_SRGB variant (the ps4-gcn-textured-quad example — RGBA order → NO swap).
///
/// Decoded conservatively: `0x8000_0000` swaps, everything else does not, so the example
/// (a non-`0x8000_0000` format) stays unswapped and cannot regress.
fn scanout_swap_rb(pixel_format: u32) -> bool {
    pixel_format == 0x8000_0000
}

/// Whether a Vulkan format carries the sRGB transfer function (auto decode-on-sample /
/// encode-on-store). Used to decide if the present must encode linear→sRGB itself: an
/// `_SRGB` swapchain encodes linear->sRGB on store, so the present shader must first DECODE
/// the gamma-space `texture_image` sample (`decode_srgb = 1`) for the two to cancel; a
/// non-`_SRGB` swapchain stores raw and needs no correction (task-154 residual #2,
/// task-175). The corpus only reaches the 8-bit surface formats plus the 2-10-10-10 HDR
/// fallback.
fn format_is_srgb(format: vk::Format) -> bool {
    matches!(
        format,
        vk::Format::R8G8B8A8_SRGB | vk::Format::B8G8R8A8_SRGB
    )
}

/// Sentinel handed to [`GpuBackend::present`] meaning "the target most recently
/// submitted via [`AshBackend::submit_flip`]". The videoout framebuffer is the
/// only target today, tracked by its `(handle, index)` inside the backend; the
/// opaque [`TargetId`] keeps the trait Vulkan-free without a lossy encoding.
pub const CURRENT_TARGET: TargetId = TargetId(0);

/// The ash present backend. Sole impl of [`GpuBackend`] (doc-2 §2 keeps one impl).
pub struct AshBackend {
    ctx: VulkanContext,
    guest_memory: Arc<RwLock<Box<dyn VirtualMemoryManager>>>,

    // task-136: queryable device capabilities, snapshotted from the VulkanContext at
    // construction so the backend/present path can consult them without reaching back
    // into `ctx`. This is the seam for a future caps-tiered Vulkan/MoltenVK split
    // (decision-3/6/7); nothing gates on it yet — the portable baseline runs
    // everywhere (behavior unchanged). A caps-gated fast path is a later, MEASURED
    // optimization. Read via `caps()`.
    caps: ps4_core::gpu::GpuCaps,

    buffers: HashMap<(i32, u32), DisplayBuffer>,
    // Zero-copy import cache, keyed by (handle, index). An entry exists
    // only for buffers successfully imported via VK_EXT_external_memory_host; for
    // every other registered buffer the staging-copy path is used.
    imported: HashMap<(i32, u32), ImportedBuf>,
    current_target: Option<(i32, u32)>,
    pending_vsync_signal: Option<Sender<()>>,

    // Aggregate profiler flag: resolved once at construction.
    prof: bool,
    // Fresh ids handed out by the create_target stub until real targets land.
    next_id: u32,
    // The display-side id -> vk buffer map for the resource cache (doc-2 §8, §3).
    // Ids are minted GUEST-SIDE by the `ps4-gnm` ResourceCache and handed in via
    // create_resource / try_import_host_range; the backend only records the vk::Buffer
    // it allocated (or imported) under each id. See `GpuBackend::create_resource` and
    // the `ps4-gnm::cache` module doc for the id-ownership rationale.
    resources: HashMap<u32, CacheBuffer>,
    // The display-side id -> sampled image map (doc-2 §C3/§C4). Ids are minted GUEST-SIDE
    // like `resources`: a `CreateImage` allocates the vk::Image + view + memory under its
    // id, an `UploadImage` stages detiled linear pixels into it, and a `BindTexture` reads
    // the view back to write a combined image-sampler descriptor. Leaked on exit.
    images: HashMap<u32, CacheImage>,
    // The display-side id -> offscreen render-target map (doc-2 §8.5, task-56 RT-as-texture).
    // Ids are minted GUEST-SIDE by the `ps4-gnm` resource cache's RenderTarget key: a
    // `CreateRenderTarget` allocates the vk::Image + view + memory (one image both roles:
    // COLOR_ATTACHMENT | SAMPLED | TRANSFER_SRC) under its id. Unlike `images`, no
    // `UploadImage` ever writes it — the GPU fills it (step 4). A `BindTexture` may resolve
    // its `image_id` here so a draw samples an RT as a texture with zero upload. Leaked on
    // exit like the rest of the Vulkan state.
    render_targets: HashMap<u32, CacheRenderTarget>,
    /// Render targets whose readback has already been refused, so the refusal is reported
    /// ONCE per target instead of once per frame (task-181). A refusal is a property of the
    /// surface's geometry, not of the frame, so repeating it every flip says nothing new and
    /// buries every other line in the log — which defeats the point of a diagnostic.
    readback_refused: std::collections::HashSet<u32>,
    // The display-side id -> sampler map (doc-2 §C4). A `CreateSampler` records a
    // vk::Sampler under its guest-minted id; a `BindTexture` reads it back. Leaked on exit.
    samplers: HashMap<u32, vk::Sampler>,

    // ---- host-pipeline draw (doc-2 §4, decision-7) ----
    // The display-side pipeline cache: `PipelineId -> vk::Pipeline`. Ids are minted
    // GUEST-SIDE by the `ps4-gnm` pipeline cache and handed in via `CreatePipeline`
    // (which carries the SPIR-V the pipeline is built from); a `BindPipeline { id }`
    // looks the built pipeline up here. The executor never names a `vk::` type — SPIR-V
    // crosses as `Arc<[u32]>`. Built once per id, reused every frame; leaked on exit.
    pipelines: HashMap<u32, HostPipeline>,
    // The render pass + framebuffer that render draws INTO the videoout `texture_image`
    // the present path then blits to the swapchain. Created lazily on the first draw
    // (it depends only on the fixed videoout target, not the shaders).
    draw_target: Option<EmbeddedTarget>,
    // Set once a draw has populated `texture_image`, and LATCHED across presents: makes
    // every subsequent `present()` skip the guest-framebuffer buffer→image copy (which
    // would overwrite the drawn pixels) and blit the drawn image straight to the swapchain.
    // A homebrew that draws through the GPU path submits its scene once but the display
    // loop presents continuously; if this were consumed after one present, the drawn frame
    // would flash for a single vsync and then be overwritten by the (empty) guest
    // framebuffer copy. The latch is re-armed by every draw list (the next draw overwrites
    // `texture_image`), so the newest drawn frame is what scans out. A pure softgpu title
    // never records a draw, so this stays false for it and the guest-copy present path is
    // unaffected.
    embedded_drawn: bool,
    // Whether the videoout `texture_image` has already been CLEARed this GUEST FRAME
    // (task-152). A guest frame is a run of submits terminated by a flip; Celeste splits
    // its ~499 videoout draws across MANY submits per frame. Each `record_passes` call is
    // ONE submit and its local `videoout_seen` flag makes only the FIRST videoout pass IN
    // THAT SUBMIT clear (later passes in the same submit LOAD, task-149). Without a
    // frame-scoped latch, the FIRST videoout pass of EVERY submit re-clears, erasing all
    // geometry drawn by prior submits in the same frame — so only the last videoout
    // submit's draws survive. This latch makes only the first videoout pass of the FIRST
    // videoout submit in a frame clear; every later submit's videoout passes LOAD and
    // accumulate. Reset to false at the guest flip (`submit_flip`) so the next frame
    // clears fresh. Unlike the offscreen RT path (which tracks `current_layout` per RT),
    // the shared videoout target has no per-image layout state, hence this explicit latch.
    videoout_cleared_this_frame: bool,
    // A dedicated fence the whole-submit draw list is submitted with, so the display
    // thread waits on THIS list's completion (not a global `device_wait_idle`) before
    // the present path reads `texture_image`. Created lazily; reset+reused per list.
    draw_fence: Option<vk::Fence>,
    // The suballocating allocator every copy-path cache buffer is carved from (task-223).
    pool: crate::buffer_pool::CacheBufferPool,
    // Cache buffers a `FreeResource` released during the CURRENT command list. They are not
    // returned to the pool immediately: an earlier draw in this same list may still bind
    // one, and that draw is not submitted until the end of the walk. The queue is drained
    // into the pool at the START of the next list, by which point this list has been
    // submitted AND its fence waited on inside `record_passes`, so no GPU work can still be
    // reading them.
    pending_recycle: Vec<crate::buffer_pool::PooledBuffer>,
    // Env-gated visual oracle: when `UNEMUPS4_DUMP_PNG=<path>` is set, the presented
    // swapchain image is read back to an RGBA PNG at `<path>` (a `.png` file, or a
    // directory that gets a `frame_NNNN.png` per flip). `None` = off, no readback,
    // oracle baselines unchanged. Resolved once at construction.
    dump_png: Option<DumpPng>,
}

/// Env-gated PNG dump destination (see `AshBackend::dump_png`). Off unless
/// `UNEMUPS4_DUMP_PNG` is set.
struct DumpPng {
    /// The `UNEMUPS4_DUMP_PNG` value: a file path (single-file overwrite) or a
    /// directory (per-flip `frame_NNNN.png`).
    path: std::path::PathBuf,
    is_dir: bool,
    /// Monotonic flip counter for the directory naming and for the file-mode
    /// "dump only the first N frames won't help" — files just overwrite.
    frame: u32,
}

/// A cached host graphics pipeline built from a `CreatePipeline` command's SPIR-V,
/// recorded under its guest-minted [`PipelineId`](ps4_core::gpu::PipelineId) (doc-2 §4,
/// decision-7). Rebuilt never; dropped on process exit with the rest of the
/// leak-on-exit Vulkan state.
struct HostPipeline {
    pipeline: vk::Pipeline,
    // The pipeline layout, read back when a storage-buffer draw binds its descriptor set
    // and pushes num_records (leak-on-exit like the rest of the Vulkan state).
    layout: vk::PipelineLayout,
    // The descriptor-set layout the record pass allocates a set against, `Some` only for a
    // recompiled-VS pipeline that fetches through an SSBO. `None` for the embedded path
    // (empty pipeline layout, gl_VertexIndex, no descriptors). Leaked on exit.
    set_layout: Option<vk::DescriptorSetLayout>,
    // How many constant-buffer bindings the pipeline was built with: the VS const
    // (set0/bind2) and/or the PS const (set0/bind6). The draw MUST supply a resolved
    // `ConstBind` for EACH or the descriptor at the unsupplied slot is left unwritten
    // (undefined-descriptor UB).
    //
    // This is a COUNT, not a bool, and that is load-bearing (task-184): a pipeline
    // declaring both — every draw whose PS reads constants AND whose VS pulls vertices,
    // e.g. Celeste's two bloom-blur passes — passed a "did any const bind arrive?" test
    // with only ONE of the two resolved, and rendered anyway, reading whatever the driver
    // left in the other descriptor. A shader whose scalar term arrives as zero produces a
    // plausible picture, not a crash, which is why this survived.
    needs_const: u32,
    // Same contract, same counting requirement, for the vertex-pull SSBOs: one binding per
    // V# stream (task-153), so a 3-stream draw with 1 resolved stream must not record.
    needs_storage: u32,
    // Same contract, same counting requirement, for the combined image-samplers: a PS
    // declares one per distinct image_sample descriptor pair (task-199), so a 2-texture
    // draw with only 1 resolved bind must not record — the un-written descriptor is
    // undefined-descriptor UB that renders a plausible WRONG picture rather than faulting.
    needs_texture: u32,
}

/// The render pass + framebuffer that target the videoout `texture_image` for a draw.
/// Shared by all pipelines (one fixed color target).
struct EmbeddedTarget {
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,
}

/// A host-visible linear buffer backing one resource-cache entry (doc-2 §8.2 copy
/// path). Held live in `AshBackend::resources` under its guest-minted id; leaked on
/// exit with the rest of the Vulkan state. `mem` is retained so `upload` can map it.
struct CacheBuffer {
    // The buffer is bound as a vertex/index buffer at draw time and kept alive (VRAM
    // stays live until evicted). It is carved from a shared pool block (task-223), so it
    // owns no device allocation of its own; `pooled` is what goes back on the pool's free
    // list when the resource is released.
    pooled: crate::buffer_pool::PooledBuffer,
    // The size the cache asked for, bounding the upload memcpy (task-222): offsets and
    // sizes come from the guest, and the pool's size class is generally LARGER than the
    // request, so the class cannot serve as the bound — an upload past the requested size
    // would be writing bytes no descriptor covers.
    len: u64,
}

impl CacheBuffer {
    fn buffer(&self) -> vk::Buffer {
        self.pooled.buffer
    }

    /// The persistent host mapping of the buffer's bytes (task-222). Established once with
    /// the pool block and held for the block's lifetime, so an `UploadBuffer` is a bare
    /// memcpy — the per-upload map/unmap pair cost more than the copy it wrapped.
    fn ptr(&self) -> *mut u8 {
        self.pooled.ptr
    }
}

/// A device-local sampled image backing one texture-cache entry (doc-2 §C3/§C4). Held live
/// in `AshBackend::images` under its guest-minted id; leaked on exit. `view` is what a
/// `BindTexture` writes into a combined image-sampler descriptor; `extent` sizes the
/// staging copy an `UploadImage` records.
struct CacheImage {
    image: vk::Image,
    view: vk::ImageView,
    #[allow(dead_code)] // backing memory retained so the image stays valid; leaked on exit.
    mem: vk::DeviceMemory,
    extent: vk::Extent2D,
}

/// An offscreen render target backing one `RenderTarget` resource-cache entry (doc-2 §8.5,
/// task-56 RT-as-texture). Held live in `AshBackend::render_targets` under its guest-minted
/// id; leaked on exit. Unlike [`CacheImage`], the same `vk::Image` is BOTH a color attachment
/// (a draw renders into it, step 4) and a sampled source (`view` a later draw binds through a
/// combined image-sampler) — the one-image-both-roles pattern proven portable by the videoout
/// `texture_image`. `current_layout` tracks the RT's layout ACROSS passes/submits (an RT
/// changes layout — UNDEFINED on create, COLOR_ATTACHMENT while drawn, SHADER_READ while
/// sampled — unlike a `CacheImage` which is always SHADER_READ), so the multi-pass refactor
/// (step 4) can pick the correct initial layout for a re-render without discarding
/// cross-frame accumulation. `extent` sizes the RT's own render pass/framebuffer (step 4).
struct CacheRenderTarget {
    image: vk::Image,
    view: vk::ImageView,
    #[allow(dead_code)] // backing memory retained so the image stays valid; leaked on exit.
    mem: vk::DeviceMemory,
    // Sizes the RT's own render pass/framebuffer + render-area/viewport fallback (step 4).
    extent: vk::Extent2D,
    // The RT's current Vulkan image layout, tracked across passes and submits. Created
    // UNDEFINED; the step-4 write pass reads this to choose its initial layout (CLEAR from
    // UNDEFINED on first use, else LOAD from SHADER_READ) and updates it after the
    // color→shader-read barrier. Mandatory — hardcoding UNDEFINED on reuse would discard
    // cross-frame accumulation and risk a layout-desync validation fault; a fence-timeout
    // rolls it back to UNDEFINED so the next submit clears rather than LOADs a stale layout.
    current_layout: vk::ImageLayout,
}

/// The final draw a submit's command list records: a non-indexed `vkCmdDraw` (embedded
/// fullscreen quad / recompiled VS) or an indexed `vkCmdDrawIndexed` over a cached index
/// buffer.
enum DrawCall {
    Auto {
        vertex_count: u32,
    },
    Indexed {
        buffer: vk::Buffer,
        index_count: u32,
        index_type: IndexType,
    },
}

/// One vertex buffer a draw binds (`BindVertexBuffer`), resolved to its cached
/// `vk::Buffer` for `vkCmdBindVertexBuffers`.
struct VertexBinding {
    slot: u32,
    buffer: vk::Buffer,
    #[allow(dead_code)] // stride is baked into the pipeline vertex-input; kept for parity.
    stride: u32,
}

/// One vertex-pull SSBO a recompiled-VS draw binds (`BindStorageBuffer`), resolved to its
/// cached `vk::Buffer`. Multi-stream vertex fetch (task-153) binds up to 4 of these per draw
/// — one per V# stream. The record pass allocates one descriptor set against the bound
/// pipeline's set layout, points each stream's `binding` at its buffer, and pushes that
/// stream's `num_records` (fetch clamp) + `stride` (vertex element stride) + `dst_sel`
/// (destination swizzle) + `format` (packed dfmt/nfmt) 4-uint group at `pc_offset` before
/// the draw.
struct StorageBind {
    #[allow(dead_code)] // set 0 is the only descriptor set today; kept for parity.
    set: u32,
    binding: u32,
    buffer: vk::Buffer,
    num_records: u32,
    /// The V#'s per-element stride in BYTES, pushed as this stream's push-constant member 1
    /// (`pc_offset + 4`) so the recompiled VS addresses a non-16 stride correctly (task-140).
    stride: u32,
    /// The V#'s packed destination swizzle (`word3[11:0]`), pushed as this stream's
    /// push-constant member 2 (`pc_offset + 8`) so the recompiled VS applies the per-channel
    /// 0.0/1.0/source substitution GCN's format/swizzle stage does (task-155).
    dst_sel: u32,
    /// The V#'s packed vertex FORMAT (`dfmt` in `[7:0]`, `nfmt` in `[15:8]`), pushed as this
    /// stream's push-constant member 3 (`pc_offset + 12`) so the recompiled VS unpacks each
    /// fetched component per the format — packed `_8_8_8_8` UNORM / `_16*` decode vs the raw
    /// 32-bit-float dword (task-164).
    format: u32,
    /// Byte offset into the pipeline's push-constant range where this stream's 4-uint group
    /// ({num_records, stride, dst_sel, format}) is written (= `16*stream`; `0` single-stream,
    /// task-153/task-164).
    pc_offset: u32,
}

/// The scalar constant-buffer SSBO a recompiled-VS draw binds (`BindConstBuffer`), resolved
/// to its cached `vk::Buffer`. The record pass writes it into the same set-0 descriptor set
/// as the vertex-pull `StorageBind`, at `binding` (VERTEX stage), before the draw. No
/// `num_records` — a constant buffer is a flat `uint[]` the VS indexes by dword offset.
struct ConstBind {
    #[allow(dead_code)] // set 0 is the only descriptor set today; kept for parity.
    set: u32,
    binding: u32,
    buffer: vk::Buffer,
}

/// The one combined image-sampler a textured draw binds (`BindTexture`), resolved to its
/// cached image `view` + `sampler`. The record pass writes it into the pipeline's set-0
/// descriptor at `binding` (FRAGMENT stage) before the draw so the pixel shader can sample
/// it.
struct TextureBind {
    #[allow(dead_code)] // set 0 is the only descriptor set today; kept for parity.
    set: u32,
    binding: u32,
    view: vk::ImageView,
    sampler: vk::Sampler,
}

/// Which target a single [`RecordedPass`] renders into (doc-2 §8.5, task-56 step 4). A
/// submit's command list is a SEQUENCE of passes: zero or more offscreen producer passes
/// followed by the final videoout pass. The offscreen passes are recorded FIRST (they fill
/// render targets a later pass samples); the videoout pass stays LAST so the present path
/// is unchanged.
enum PassTarget {
    /// The fixed videoout `texture_image` (the present path blits it to the swapchain).
    /// Only a videoout pass sets the `embedded_drawn` latch.
    Videoout,
    /// An offscreen render target, named by its guest-minted id (a key into
    /// `render_targets`). The pass records into the RT's own render pass + framebuffer
    /// (sized to the RT extent) and, after the draw, barriers the RT
    /// COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL so a later pass can sample it.
    RenderTarget(u32),
}

/// One resolved draw ready to record into the shared command buffer (doc-2 §8.5, task-56
/// step 4). The multi-pass refactor accumulates the per-draw state (pipeline + binds +
/// viewport/scissor + the draw itself) into one of these at each `DrawAuto`/`DrawIndexed`,
/// tagged with the target the draw hits. All passes for a submit record into ONE command
/// buffer behind ONE fence.
struct RecordedPass {
    target: PassTarget,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    set_layout: Option<vk::DescriptorSetLayout>,
    draw: DrawCall,
    vertex_buffers: Vec<VertexBinding>,
    /// The vertex-pull SSBO binds for this draw, one per V# stream (task-153). Empty for an
    /// embedded / non-vertex-pull draw.
    storage_binds: Vec<StorageBind>,
    /// The constant-buffer binds for this draw — up to two (task-174): the VS const at
    /// set0/bind2 and the PS const at set0/bind6. Empty for a draw that loads no constants.
    const_binds: Vec<ConstBind>,
    texture_binds: Vec<TextureBind>,
    viewport: Option<ViewportRect>,
    scissor: Option<ScissorRect>,
}

impl AshBackend {
    pub fn new(
        ctx: VulkanContext,
        guest_memory: Arc<RwLock<Box<dyn VirtualMemoryManager>>>,
    ) -> Self {
        Self {
            // task-136: snapshot caps before `ctx` is moved into the struct.
            caps: ctx.caps,
            ctx,
            guest_memory,
            buffers: HashMap::new(),
            imported: HashMap::new(),
            current_target: None,
            pending_vsync_signal: None,
            prof: present_profile::enabled(),
            next_id: 1,
            resources: HashMap::new(),
            images: HashMap::new(),
            render_targets: HashMap::new(),
            readback_refused: std::collections::HashSet::new(),
            samplers: HashMap::new(),
            pipelines: HashMap::new(),
            draw_target: None,
            embedded_drawn: false,
            videoout_cleared_this_frame: false,
            draw_fence: None,
            pool: crate::buffer_pool::CacheBufferPool::default(),
            pending_recycle: Vec::new(),
            dump_png: std::env::var_os("UNEMUPS4_DUMP_PNG").map(|v| {
                let path = std::path::PathBuf::from(v);
                // A trailing-slash or existing directory means per-flip frame files;
                // anything else is a single file overwritten each flip.
                let is_dir = path.is_dir()
                    || path.extension().is_none()
                    || path.to_string_lossy().ends_with('/');
                DumpPng {
                    path,
                    is_dir,
                    frame: 0,
                }
            }),
        }
    }

    /// Replay one submit's `BackendCmd` list into the videoout `texture_image`
    /// (doc-2 §3, decision-7). Runs on the display thread that owns the device.
    ///
    /// A `CreatePipeline` builds a `vk::Pipeline` from the carried SPIR-V and records it
    /// under its guest-minted id; a `BindPipeline { id }` selects it; a `DrawAuto`
    /// records the draw. The whole list is recorded into ONE command buffer and
    /// submitted with a per-list fence — no per-draw `device_wait_idle` — so O(draws)
    /// full-GPU stalls per frame collapse to one wait (doc-2 §3 data-list model). The
    /// target is left in `SHADER_READ_ONLY_OPTIMAL` so the next `present()` blit reads
    /// it. A list with no bound-pipeline draw records nothing (the draw was deferred
    /// upstream).
    pub fn run_command_list(&mut self, cmds: &[BackendCmd]) {
        let _span = tracing::debug_span!("gnm_submit").entered();
        let t = self.prof.then(Instant::now);
        self.replay_command_list(cmds);
        if let Some(t) = t {
            SUBMIT
                .backend_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    fn replay_command_list(&mut self, cmds: &[BackendCmd]) {
        // Return the buffers the PREVIOUS list released to the pool (task-223). That list
        // was submitted and its fence waited on before this call, so nothing the GPU reads
        // can still name them; doing it here rather than at the free keeps the eviction
        // path off the fence entirely. See `free_resource`.
        for buf in std::mem::take(&mut self.pending_recycle) {
            // SAFETY: display thread owns the device; the previous list's fence was waited
            // on before this call, so no submit reading `buf` is outstanding.
            unsafe { self.pool.recycle(&self.ctx, buf) };
        }
        // A submit's command list is a SEQUENCE of draws, each rendering into either an
        // offscreen render target (a producer pass whose `SetRenderTarget` named the RT) or
        // the fixed videoout target (task-56 step 4). Walk the list once: apply the
        // resource-cache + pipeline-create commands (these mutate the backend's own maps),
        // accumulate the per-draw state, and CLOSE a `RecordedPass` at each `DrawAuto`/
        // `DrawIndexed`. Every draw re-binds its own pipeline + resources + viewport/scissor
        // upstream, so the per-draw accumulators reset after each pass. All the passes then
        // record into ONE command buffer behind ONE fence.
        let mut bound: Option<vk::Pipeline> = None;
        // The bound pipeline's layout + descriptor-set layout, carried alongside the
        // pipeline so the record pass can bind descriptors / push constants for an SSBO
        // draw. `set_layout` is `Some` only for a recompiled-VS pipeline built with a
        // storage binding; the embedded path has an empty layout and no set.
        let mut bound_layout: Option<vk::PipelineLayout> = None;
        let mut bound_set_layout: Option<vk::DescriptorSetLayout> = None;
        let mut bound_needs_const = 0u32;
        let mut bound_needs_storage = 0u32;
        let mut bound_needs_texture = 0u32;
        let mut vertex_buffers: Vec<VertexBinding> = Vec::new();
        // Multi-stream vertex fetch (task-153): one bind per V# stream, pushed by each
        // `BindStorageBuffer`. Empty for the embedded / non-vertex-pull path.
        let mut storage_binds: Vec<StorageBind> = Vec::new();
        // Up to two const binds per draw (task-174): the VS const and the PS const, each
        // pushed by its own `BindConstBuffer`. Empty for a draw that loads no constants.
        let mut const_binds: Vec<ConstBind> = Vec::new();
        let mut texture_binds: Vec<TextureBind> = Vec::new();
        let mut viewport: Option<ViewportRect> = None;
        let mut scissor: Option<ScissorRect> = None;
        // The render target the NEXT draw writes into (task-56 step 4). Set by
        // `SetRenderTarget`, consumed + cleared by the draw it precedes — a draw with no
        // preceding `SetRenderTarget` renders into the videoout target (present path
        // unchanged). Never carries past one draw.
        let mut current_rt: Option<u32> = None;
        // The resolved passes this submit records, in stream order (offscreen producers
        // first, the videoout draw last).
        let mut passes: Vec<RecordedPass> = Vec::new();
        // task-56 step 5: RT readbacks requested this submit (ReadbackPolicy::All only). Run
        // AFTER `record_passes` so the RT is in SHADER_READ (rendered), not mid-flight. Empty
        // under the default Off policy — the executor emits no ReadbackRenderTarget then.
        let mut readbacks: Vec<(u32, u64, u64, u32, ps4_core::gpu::Tiling)> = Vec::new();
        // task-187: RT PNG dumps requested this submit by an armed GPU-state snapshot. Run
        // after `record_passes` for the same reason as the readbacks above, but kept a
        // SEPARATE list because it is a separate job — a host-image copy for a human to
        // look at, with no guest layout to satisfy and no guest memory to write. Empty
        // unless a capture is armed and `UNEMUPS4_SNAPSHOT_RENDER_TARGETS` is on.
        let mut rt_dumps: Vec<(u32, std::path::PathBuf)> = Vec::new();
        let t_walk = self.prof.then(Instant::now);
        let _walk_span = tracing::debug_span!("cmd_walk").entered();
        // task-222: the per-variant breakdown of this walk. One clock read per command,
        // reused as the next command's start, so the breakdown costs one `Instant::now`
        // per command instead of a bracketing pair.
        let mut last = t_walk;
        for cmd in cmds {
            match cmd {
                BackendCmd::CreatePipeline {
                    id,
                    vs_spirv,
                    ps_spirv,
                    key,
                    target,
                    vertex_storage,
                    push_constants,
                    textures,
                    const_storage,
                    const_storage_fragment,
                } => {
                    self.create_pipeline(
                        *id,
                        vs_spirv,
                        ps_spirv,
                        key,
                        target,
                        vertex_storage,
                        *push_constants,
                        textures,
                        *const_storage,
                        *const_storage_fragment,
                    );
                }
                &BackendCmd::BindPipeline { id } => {
                    if let Some(p) = self.pipelines.get(&id.0) {
                        bound = Some(p.pipeline);
                        bound_layout = Some(p.layout);
                        bound_set_layout = p.set_layout;
                        bound_needs_const = p.needs_const;
                        bound_needs_storage = p.needs_storage;
                        bound_needs_texture = p.needs_texture;
                    } else {
                        bound = None;
                        bound_layout = None;
                        bound_set_layout = None;
                        bound_needs_const = 0;
                        bound_needs_storage = 0;
                        bound_needs_texture = 0;
                    }
                }
                &BackendCmd::DrawAuto { vertex_count } => {
                    // Close a pass at this draw. The per-draw undefined-descriptor guards
                    // (a declared binding whose resource missed the cache) drop THIS draw
                    // only — a later draw in the same submit still records. On a drop or a
                    // missing pipeline the accumulators still reset so the next draw starts
                    // clean.
                    if let (Some(pipeline), true) = (
                        bound,
                        draw_guards_ok(
                            bound,
                            bound_needs_const,
                            const_binds.len() as u32,
                            bound_needs_storage,
                            storage_binds.len() as u32,
                            bound_needs_texture,
                            texture_binds.len() as u32,
                        ),
                    ) {
                        passes.push(RecordedPass {
                            target: match current_rt {
                                Some(id) => PassTarget::RenderTarget(id),
                                None => PassTarget::Videoout,
                            },
                            pipeline,
                            pipeline_layout: bound_layout.unwrap_or(vk::PipelineLayout::null()),
                            set_layout: bound_set_layout,
                            draw: DrawCall::Auto { vertex_count },
                            vertex_buffers: std::mem::take(&mut vertex_buffers),
                            storage_binds: std::mem::take(&mut storage_binds),
                            const_binds: std::mem::take(&mut const_binds),
                            texture_binds: std::mem::take(&mut texture_binds),
                            viewport,
                            scissor,
                        });
                    }
                    vertex_buffers.clear();
                    storage_binds.clear();
                    const_binds.clear();
                    texture_binds.clear();
                    viewport = None;
                    scissor = None;
                    current_rt = None;
                }
                &BackendCmd::BindVertexBuffer { slot, id, stride } => {
                    if let Some(res) = self.resources.get(&id.0) {
                        vertex_buffers.push(VertexBinding {
                            slot,
                            buffer: res.buffer(),
                            stride,
                        });
                    }
                }
                &BackendCmd::BindStorageBuffer {
                    set,
                    binding,
                    id,
                    num_records,
                    stride,
                    dst_sel,
                    format,
                    pc_offset,
                } => {
                    // Multi-stream (task-153): one command per V# stream — PUSH into the vec
                    // so an N-stream VS accumulates all N binds before the draw closes them.
                    if let Some(res) = self.resources.get(&id.0) {
                        storage_binds.push(StorageBind {
                            set,
                            binding,
                            buffer: res.buffer(),
                            num_records,
                            stride,
                            dst_sel,
                            format,
                            pc_offset,
                        });
                    }
                }
                &BackendCmd::BindConstBuffer { set, binding, id } => {
                    // PUSH into the vec so a draw whose VS AND PS both load constants (task-174)
                    // accumulates both binds before the draw closes them, each at its own
                    // binding (VS set0/bind2, PS set0/bind6).
                    if let Some(res) = self.resources.get(&id.0) {
                        const_binds.push(ConstBind {
                            set,
                            binding,
                            buffer: res.buffer(),
                        });
                    }
                }
                &BackendCmd::DrawIndexed {
                    id,
                    index_count,
                    index_type,
                } => {
                    let index_buffer = self.resources.get(&id.0).map(|res| res.buffer());
                    if let (Some(pipeline), Some(buffer), true) = (
                        bound,
                        index_buffer,
                        draw_guards_ok(
                            bound,
                            bound_needs_const,
                            const_binds.len() as u32,
                            bound_needs_storage,
                            storage_binds.len() as u32,
                            bound_needs_texture,
                            texture_binds.len() as u32,
                        ),
                    ) {
                        passes.push(RecordedPass {
                            target: match current_rt {
                                Some(rt) => PassTarget::RenderTarget(rt),
                                None => PassTarget::Videoout,
                            },
                            pipeline,
                            pipeline_layout: bound_layout.unwrap_or(vk::PipelineLayout::null()),
                            set_layout: bound_set_layout,
                            draw: DrawCall::Indexed {
                                buffer,
                                index_count,
                                index_type,
                            },
                            vertex_buffers: std::mem::take(&mut vertex_buffers),
                            storage_binds: std::mem::take(&mut storage_binds),
                            const_binds: std::mem::take(&mut const_binds),
                            texture_binds: std::mem::take(&mut texture_binds),
                            viewport,
                            scissor,
                        });
                    }
                    vertex_buffers.clear();
                    storage_binds.clear();
                    const_binds.clear();
                    texture_binds.clear();
                    viewport = None;
                    scissor = None;
                    current_rt = None;
                }
                &BackendCmd::SetViewport(v) => viewport = Some(v),
                &BackendCmd::SetScissor(s) => scissor = Some(s),
                &BackendCmd::CreateBuffer { id, size } => {
                    self.create_resource(id, &ResourceDesc { size });
                }
                BackendCmd::UploadBuffer { id, offset, data } => {
                    self.upload(*id, *offset, data);
                }
                &BackendCmd::ImportBuffer { id, addr, size } => {
                    self.replay_import(id, addr, size);
                }
                &BackendCmd::FreeResource { id } => {
                    self.free_resource(id);
                }
                &BackendCmd::CreateImage {
                    id,
                    width,
                    height,
                    format,
                } => {
                    self.create_image(id, width, height, format);
                }
                BackendCmd::UploadImage { id, data } => {
                    self.upload_image(*id, data);
                }
                &BackendCmd::CreateSampler { id, desc } => {
                    self.create_sampler(id, desc);
                }
                &BackendCmd::BindTexture {
                    set,
                    binding,
                    image_id,
                    sampler_id,
                } => {
                    // task-56 step 3: resolve `image_id` from EITHER a plain sampled image
                    // (`images`, an `UploadImage`-fed texture) OR an offscreen render target
                    // (`render_targets`, GPU-filled, RT-as-texture). One `BindTexture` serves
                    // both — a plain texture binds its `CacheImage.view`, an RT binds its
                    // `CacheRenderTarget.view` (same COLOR-aspect 2D view). An id in neither
                    // map leaves this bind unrecorded → the needs_texture count guard defers
                    // the draw rather than leaving a descriptor un-written.
                    let view = self
                        .images
                        .get(&image_id.0)
                        .map(|img| img.view)
                        .or_else(|| self.render_targets.get(&image_id.0).map(|rt| rt.view));
                    // task-179 trace (`UNEMUPS4_X_PASS_TRACE=1`, default off): which map the
                    // sampled id resolved from, and the RT id this pass draws into. The bloom
                    // chain reads nothing from the scene RT; this says whether the draw even
                    // binds the right image before any synchronisation question arises.
                    static PASS_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                    if *PASS_TRACE.get_or_init(|| {
                        std::env::var("UNEMUPS4_X_PASS_TRACE").is_ok_and(|v| v != "0")
                    }) {
                        let kind = if self.images.contains_key(&image_id.0) {
                            "image"
                        } else if self.render_targets.contains_key(&image_id.0) {
                            "RT"
                        } else {
                            "UNRESOLVED"
                        };
                        tracing::info!(
                            "[X_PASS] bind tex id={} from={kind} into_rt={:?}",
                            image_id.0,
                            current_rt
                        );
                    }
                    if let (Some(view), Some(&sampler)) = (view, self.samplers.get(&sampler_id.0)) {
                        // One entry per BindTexture: a multi-texture PS emits one per
                        // declared binding (task-199). Replace an entry for the same
                        // binding rather than appending a duplicate.
                        let tb = TextureBind {
                            set,
                            binding,
                            view,
                            sampler,
                        };
                        match texture_binds.iter_mut().find(|b| b.binding == binding) {
                            Some(slot) => *slot = tb,
                            None => texture_binds.push(tb),
                        }
                    }
                }
                // task-56 step 3: allocate the offscreen RT image so it EXISTS and is
                // BINDABLE (a later `BindTexture` resolves its id from `render_targets`). The
                // RT is not yet rendered into — that is the step-4 multi-pass refactor — so a
                // draw that both creates and samples an RT this same submit sees a cleared
                // (empty) image, which is correct for this step (the existing needs_texture
                // defer no longer fires for a registered RT, but the image is empty).
                &BackendCmd::CreateRenderTarget {
                    id,
                    width,
                    height,
                    format,
                } => {
                    self.create_render_target(id, width, height, format);
                }
                // task-56 step 5: queue the readback, keyed to the RT id. Deferred to after
                // `record_passes` (below) — the RT must be rendered + in SHADER_READ before it
                // is copied out. Only ever emitted by the executor under ReadbackPolicy::All.
                &BackendCmd::ReadbackRenderTarget {
                    id,
                    addr,
                    size,
                    pitch,
                    tiling,
                } => {
                    readbacks.push((id.0, addr, size, pitch, tiling));
                }
                // task-187: queue the diagnostic PNG dump, likewise deferred to after
                // `record_passes`. The path was chosen by the submit thread (only it knows
                // the capture's frame directory); the display thread only copies and writes.
                BackendCmd::DumpRenderTargetPng { id, path } => {
                    rt_dumps.push((id.0, path.clone()));
                }
                // task-56 step 4: the next draw renders INTO this offscreen RT. Applies to
                // exactly one draw — the draw arm consumes and clears `current_rt`. An id
                // with no matching `render_targets` entry is tolerated: the pass records into
                // videoout instead of an absent RT (the resolve happens at record time).
                &BackendCmd::SetRenderTarget { id } => {
                    current_rt = Some(id.0);
                }
            }
            if let Some(t) = last {
                let now = Instant::now();
                let i = present_profile::cmd_index(cmd);
                present_profile::CMDS.count[i].fetch_add(1, Ordering::Relaxed);
                present_profile::CMDS.ns[i]
                    .fetch_add(now.duration_since(t).as_nanos() as u64, Ordering::Relaxed);
                if let BackendCmd::UploadBuffer { data, .. } = cmd {
                    present_profile::CMDS
                        .upload_bytes
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                last = Some(now);
            }
        }
        if let Some(t) = t_walk {
            SUBMIT
                .walk_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        drop(_walk_span);
        // Nothing resolved (every draw deferred, or a pure state list) → record nothing. A
        // readback can only accompany a producer draw (the executor queues it alongside the
        // SetRenderTarget that opens a pass), so an empty pass list has nothing to read back.
        if passes.is_empty() {
            return;
        }
        // The videoout render pass/framebuffer is built lazily on first pipeline create; if
        // no pipeline ever created it (all-offscreen list before the first videoout draw),
        // build it now so the record path has a target for any videoout pass.
        if self.draw_target.is_none() {
            // SAFETY: device owned by this thread; texture_image/texture_view are live.
            let created = unsafe { create_embedded_target(&self.ctx) };
            self.draw_target = Some(created);
        }
        // Fence first (needs `&mut self`). One fence guards the whole multi-pass submit.
        let fence = self.ensure_draw_fence();
        // Record every pass into ONE command buffer / ONE submit behind that fence, in
        // stream order (offscreen producers first, the videoout draw last). Each offscreen
        // pass leaves its RT in SHADER_READ so a later pass samples it; `current_layout` is
        // tracked across submits in `render_targets`. Returns whether a videoout pass drew
        // (only that arms the present latch) and, on fence-timeout, forces the touched RTs'
        // `current_layout` back to UNDEFINED so a desync cannot fault the next submit.
        let videoout_drawn = self.record_passes(&passes, fence);
        // The present blit for this flip must read the drawn image, not overwrite it with the
        // guest framebuffer copy — but ONLY a videoout pass populated `texture_image`. An
        // all-offscreen list (produces RTs but nothing to present this submit) must NOT arm
        // the latch, or the present path would blit a stale/empty videoout image.
        if videoout_drawn {
            self.embedded_drawn = true;
        }
        // task-56 step 5: with the passes recorded + fence waited, every touched RT is in
        // SHADER_READ. Copy each requested RT out, re-tile to the guest surface layout, and
        // write the guest range. Off-gated upstream (empty list under ReadbackPolicy::Off).
        let t_readback = self.prof.then(Instant::now);
        for (id, addr, size, pitch, tiling) in readbacks {
            self.readback(id, addr, size, pitch, tiling);
        }
        // task-187: and the diagnostic dumps, same precondition (RT rendered + in
        // SHADER_READ), entirely different job — see `dump_render_target_png`.
        for (id, path) in rt_dumps {
            self.dump_render_target_png(id, &path);
        }
        if let Some(t) = t_readback {
            SUBMIT
                .readback_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Read a rendered offscreen render target back into its guest range (doc-2 §8.5,
    /// task-56 step 5, opt-in via [`ReadbackPolicy`](ps4_core::gpu::ReadbackPolicy)). The RT
    /// is in `SHADER_READ_ONLY_OPTIMAL` (its producer pass left it there); this transitions
    /// it to `TRANSFER_SRC_OPTIMAL`, copies its texels into a host-visible staging buffer,
    /// transitions it back, then packs the linear pixels into the GUEST surface layout
    /// ([`pack_guest_surface`], the [`tile`](ps4_gnm::cache::tile::tile) inverse of the upload
    /// detile) and writes them into `[addr, addr+size)` through the SMC-observed
    /// [`write_bytes`](VirtualMemoryManager::write_bytes) seam — NOT a raw identity store, so
    /// a dirty-tracking guest observes the write. An unknown id, a headless run with no
    /// memory manager, or a short/oversized guest range fails clean (logs + no write); the
    /// readback is a best-effort GPU→CPU mirror, never fatal.
    ///
    /// # What this readback is trustworthy for (task-181)
    ///
    /// It reports the HOST RT image's bytes packed into the guest surface's own layout. It is
    /// verified (`readback_*` tests below) for **32-bpp RGBA8** targets that are **linear**
    /// (any `pitch >= width`, including a padded one) or **1D-thin micro-tiled**. For those
    /// the round trip is exact: detiling the written guest bytes reproduces the rendered
    /// content texel-for-texel.
    ///
    /// It is NOT trustworthy — and therefore REFUSES rather than writing — for **2D
    /// macro-tiled** surfaces (`tile_mode_index >= 9`), which have no re-tiler in this repo.
    /// Celeste's render targets are macro-tiled, so under this title readback now declines
    /// with a warning instead of leaving linear bytes in a macro-tiled surface: that
    /// mismatch is what made a bright bloom target read back as near-black during task-179.
    ///
    /// Two further limits are NOT handled and NOT detected: the host RT image is always
    /// created `R8G8B8A8_UNORM` (the executor hardcodes it), so a guest target declared
    /// `B8G8R8A8` reads back channel-swapped; and only 32-bpp is expressible, since the
    /// re-tiler's texel size is fixed at [`TexelSize::Bpp32`] here.
    fn readback(
        &mut self,
        id: u32,
        addr: u64,
        size: u64,
        pitch: u32,
        tiling: ps4_core::gpu::Tiling,
    ) {
        let Some((image, extent, layout)) = self
            .render_targets
            .get(&id)
            .map(|rt| (rt.image, rt.extent, rt.current_layout))
        else {
            tracing::warn!("rt readback: unknown render-target id {id}; skipping");
            return;
        };
        // The copy barriers from SHADER_READ, which is where a producer pass leaves the RT.
        // An RT whose producer draw was DEFERRED never got there, and copying it would both
        // trip a layout desync and return undefined texels — exactly the "plausible garbage"
        // this readback must not manufacture.
        if layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            tracing::warn!(
                "rt readback: render-target id {id} is {layout:?}, not SHADER_READ (its producer \
                 draw did not run this submit); skipping"
            );
            return;
        }
        let (w, h) = (extent.width, extent.height);
        // Resolve the guest layout FIRST: a surface we cannot pack must cost no GPU stall.
        let Some(surface) = guest_surface_layout(w, h, pitch, tiling) else {
            // Once per target, not once per frame: the geometry does not change between
            // flips, so repeating this says nothing new and drowns the rest of the log.
            if self.readback_refused.insert(id) {
                tracing::warn!(
                    "rt readback: render-target id {id} ({w}x{h}, pitch {pitch}, {tiling:?}) has \
                     no expressible guest layout; REFUSING the write (values would be \
                     undecodable). Reported once per target; further refusals are silent."
                );
            }
            return;
        };
        let linear_size = (w as u64) * (h as u64) * 4;
        // SAFETY: display thread owns the device; `image` is a live RT created TRANSFER_SRC-
        // capable, currently SHADER_READ. Every handle below is created and freed here.
        let linear = unsafe {
            match self.copy_rt_to_host(image, w, h, linear_size) {
                Some(px) => px,
                None => return,
            }
        };
        let tiled = match pack_guest_surface(&linear, w, h, &surface) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("rt readback: re-tile failed for id {id}: {e:?}; skipping write");
                return;
            }
        };
        // Never write past the guest range the executor named (clamp defensively rather than
        // overrun).
        let n = (size as usize).min(tiled.len());
        // Write through the SMC-observed seam, NOT a raw identity store: a dirty-tracking
        // guest must observe this GPU→CPU write. A headless run (no VMM, or the fallback whose
        // get_host_ptr returns None) fails clean here — no raw deref.
        match self.guest_memory.read() {
            Ok(mem) => {
                if let Err(e) = mem.write_bytes(addr, &tiled[..n]) {
                    tracing::warn!(
                        "rt readback: guest write to {addr:#x} ({n} bytes) failed: {e}; skipped"
                    );
                }
            }
            Err(_) => {
                tracing::warn!("rt readback: guest memory lock poisoned; skipping write");
            }
        }
    }

    /// Write a rendered offscreen render target's pixels to `path` as a PNG, for an armed
    /// GPU state snapshot (task-187). Opt-in via `UNEMUPS4_SNAPSHOT_RENDER_TARGETS`; the
    /// executor emits no `DumpRenderTargetPng` otherwise.
    ///
    /// # Why this is NOT [`Self::readback`], and must not become it
    ///
    /// [`readback`](Self::readback) exists to satisfy `sceGnm` semantics: it puts a target's
    /// contents into GUEST memory in the GUEST's surface layout, because guest code may read
    /// those bytes. It therefore has to reproduce the guest's pitch and tile mode exactly,
    /// and REFUSE what it cannot express — which since task-181 is every 2D macro-tiled
    /// target, i.e. every Celeste render target, this repo having no macro-tiler.
    ///
    /// This function has no such obligation. Nobody is going to *execute* against these
    /// bytes; a human is going to *look* at them. The host image is already linear RGBA8,
    /// which is exactly what a PNG wants, so there is no tiling question to answer and
    /// nothing to refuse for a layout reason. It reads [`Self::copy_rt_to_host`] — the same
    /// helper `readback` uses for its first step — and stops there, writing no guest memory
    /// at all. Using one function for both jobs is what made the diagnostic inherit
    /// macro-tiling and die with it.
    ///
    /// # Threading
    ///
    /// Runs on the DISPLAY thread, which owns the device — and takes no `driver()` lock, so
    /// the task-66 invariant is untouched (this is called from the command-list walk, which
    /// already runs here). The PNG encode and the file write are handed to `ps4-gnm`'s
    /// background snapshot writer rather than done inline, so the display thread pays for
    /// the copy but not for the I/O.
    ///
    /// # Cost
    ///
    /// The copy waits on the GPU, so it perturbs frame TIMING. It does not perturb frame
    /// CONTENT: it changes no draw, no binding, no register and no guest byte, and restores
    /// the RT's layout. See `ps4_core::snapshot::render_targets_enabled`.
    ///
    /// Never fatal: an unknown id, an RT whose producer draw did not run this submit, or a
    /// failed copy logs and returns. A missing file is a logged failure, never evidence that
    /// the target was empty.
    fn dump_render_target_png(&self, id: u32, path: &std::path::Path) {
        let Some((image, extent, layout)) = self
            .render_targets
            .get(&id)
            .map(|rt| (rt.image, rt.extent, rt.current_layout))
        else {
            tracing::warn!("rt dump: unknown render-target id {id}; skipping");
            return;
        };
        // Same precondition as the readback, for the same reason: the copy barriers FROM
        // SHADER_READ, and an RT whose producer draw was deferred holds undefined texels.
        // Dumping those would put a plausible-looking picture of nothing on disk.
        if layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            tracing::warn!(
                "rt dump: render-target id {id} is {layout:?}, not SHADER_READ (its producer \
                 draw did not run this submit); skipping {}",
                path.display()
            );
            return;
        }
        let (w, h) = (extent.width, extent.height);
        let size = (w as u64) * (h as u64) * 4;
        // SAFETY: display thread owns the device; `image` is a live RT created TRANSFER_SRC-
        // capable, currently SHADER_READ — the same contract `readback` calls this under.
        let Some(linear) = (unsafe { self.copy_rt_to_host(image, w, h, size) }) else {
            // `copy_rt_to_host` already logged the specific failure.
            tracing::warn!(
                "rt dump: copy failed for id {id}; {} not written",
                path.display()
            );
            return;
        };
        // Off the display thread from here: PNG encoding plus a file write is exactly the
        // kind of work this thread must not block on.
        ps4_gnm::snapshot::enqueue_png(path.to_path_buf(), w, h, linear);
    }

    /// Copy an RT image (in `SHADER_READ_ONLY_OPTIMAL`) into a fresh host-visible staging
    /// buffer and return its `w*h*4` linear RGBA8 bytes, restoring the RT to SHADER_READ. A
    /// one-shot command buffer behind its own fence (the multi-pass submit's fence was
    /// already waited). Returns `None` on any allocation/copy failure (logged), so the
    /// caller skips the guest write cleanly.
    ///
    /// # Safety
    /// The display thread must own the device and `image` must be a live RT created with
    /// `TRANSFER_SRC` usage, currently in `SHADER_READ_ONLY_OPTIMAL`.
    unsafe fn copy_rt_to_host(
        &self,
        image: vk::Image,
        w: u32,
        h: u32,
        size: u64,
    ) -> Option<Vec<u8>> {
        unsafe {
            let ctx = &self.ctx;
            let (buffer, mem) = match VulkanContext::create_buffer(
                &ctx.instance,
                &ctx.device,
                ctx.physical_device,
                size,
                vk::BufferUsageFlags::TRANSFER_DST,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            ) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("rt readback: create staging buffer failed: {e}");
                    return None;
                }
            };

            let alloc = vk::CommandBufferAllocateInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_ALLOCATE_INFO,
                command_pool: ctx.command_pool,
                level: vk::CommandBufferLevel::PRIMARY,
                command_buffer_count: 1,
                ..Default::default()
            };
            let cb = match ctx.device.allocate_command_buffers(&alloc) {
                Ok(v) => v[0],
                Err(e) => {
                    tracing::warn!("rt readback: allocate command buffer failed: {e}");
                    ctx.device.destroy_buffer(buffer, None);
                    ctx.device.free_memory(mem, None);
                    return None;
                }
            };
            let begin = vk::CommandBufferBeginInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_BEGIN_INFO,
                flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
                ..Default::default()
            };
            if let Err(e) = ctx.device.begin_command_buffer(cb, &begin) {
                tracing::warn!("rt readback: begin command buffer failed: {e}");
                let cbs = [cb];
                ctx.device.free_command_buffers(ctx.command_pool, &cbs);
                ctx.device.destroy_buffer(buffer, None);
                ctx.device.free_memory(mem, None);
                return None;
            }

            let sub = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            // SHADER_READ (where the producer pass left the RT) -> TRANSFER_SRC.
            let to_src = vk::ImageMemoryBarrier {
                s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
                src_access_mask: vk::AccessFlags::SHADER_READ,
                dst_access_mask: vk::AccessFlags::TRANSFER_READ,
                old_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                image,
                subresource_range: sub,
                ..Default::default()
            };
            ctx.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );

            let region = vk::BufferImageCopy {
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_extent: vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                },
                ..Default::default()
            };
            ctx.device.cmd_copy_image_to_buffer(
                cb,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );

            // TRANSFER_SRC -> back to SHADER_READ (the RT's tracked layout stays valid for the
            // next submit that samples it).
            let back = vk::ImageMemoryBarrier {
                s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
                src_access_mask: vk::AccessFlags::TRANSFER_READ,
                dst_access_mask: vk::AccessFlags::SHADER_READ,
                old_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                image,
                subresource_range: sub,
                ..Default::default()
            };
            ctx.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[back],
            );

            let cbs = [cb];
            if let Err(e) = ctx.device.end_command_buffer(cb) {
                tracing::warn!("rt readback: end command buffer failed: {e}");
                ctx.device.free_command_buffers(ctx.command_pool, &cbs);
                ctx.device.destroy_buffer(buffer, None);
                ctx.device.free_memory(mem, None);
                return None;
            }
            let fence = match ctx
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("rt readback: create fence failed: {e}");
                    ctx.device.free_command_buffers(ctx.command_pool, &cbs);
                    ctx.device.destroy_buffer(buffer, None);
                    ctx.device.free_memory(mem, None);
                    return None;
                }
            };
            let submit = vk::SubmitInfo {
                s_type: vk::StructureType::SUBMIT_INFO,
                command_buffer_count: 1,
                p_command_buffers: cbs.as_ptr(),
                ..Default::default()
            };
            if let Err(e) = ctx.device.queue_submit(ctx.queue, &[submit], fence) {
                tracing::warn!("rt readback: queue submit failed: {e}");
                ctx.device.destroy_fence(fence, None);
                ctx.device.free_command_buffers(ctx.command_pool, &cbs);
                ctx.device.destroy_buffer(buffer, None);
                ctx.device.free_memory(mem, None);
                return None;
            }
            // Bounded wait so a faulted copy cannot deadlock the display thread.
            let waited = ctx
                .device
                .wait_for_fences(&[fence], true, DRAW_FENCE_TIMEOUT_NS);
            let pixels = if waited.is_ok() {
                match ctx
                    .device
                    .map_memory(mem, 0, size, vk::MemoryMapFlags::empty())
                {
                    Ok(ptr) => {
                        let raw =
                            std::slice::from_raw_parts(ptr as *const u8, size as usize).to_vec();
                        ctx.device.unmap_memory(mem);
                        Some(raw)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "rt readback: map staging memory failed: {e}; skipping write"
                        );
                        None
                    }
                }
            } else {
                tracing::warn!("rt readback: copy fence timed out; skipping guest write");
                None
            };
            // Only tear down once the fence has signaled (submit complete). On the timeout
            // path the one-shot copy submit is STILL PENDING on the queue, so destroying the
            // fence / freeing the command buffer / staging buffer / memory it references is
            // undefined behavior (VUID-vkDestroyFence-fence-01120,
            // VUID-vkFreeCommandBuffers-pCommandBuffers-00047). Leak them instead — the
            // crate's leak-on-exit convention — mirroring record_passes' timeout branch.
            if waited.is_ok() {
                ctx.device.destroy_fence(fence, None);
                ctx.device.free_command_buffers(ctx.command_pool, &cbs);
                ctx.device.destroy_buffer(buffer, None);
                ctx.device.free_memory(mem, None);
            }
            pixels
        }
    }

    /// Record a submit's resolved passes into ONE command buffer, submit them behind ONE
    /// fence, and wait (doc-2 §8.5, task-56 step 4). Offscreen passes render into their RT's
    /// own render pass + framebuffer (sized to the RT extent) and end with a
    /// COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL barrier so a later pass samples
    /// the RT; the videoout pass renders into the fixed `draw_target`. The per-RT
    /// `current_layout` is read to pick each offscreen pass's initial layout (UNDEFINED +
    /// CLEAR on first use, else LOAD from SHADER_READ) and updated after its barrier. Returns
    /// `true` iff a videoout pass was recorded (the caller arms the present latch only then).
    /// On fence-timeout every RT touched this submit has its `current_layout` reset to
    /// UNDEFINED, so a partially-executed (or unexecuted) submit cannot leave a layout desync
    /// that faults the next submit's initial-layout assumption.
    fn record_passes(&mut self, passes: &[RecordedPass], fence: vk::Fence) -> bool {
        let _span = tracing::debug_span!("record_passes").entered();
        let prof = self.prof;
        let t_passes = prof.then(Instant::now);
        // The transient per-RT render pass + framebuffer objects created for this submit,
        // destroyed after the fence wait. The RT images/views themselves persist in
        // `render_targets`.
        let mut transient_rp: Vec<vk::RenderPass> = Vec::new();
        let mut transient_fb: Vec<vk::Framebuffer> = Vec::new();
        // RTs whose layout this submit advances (COLOR then SHADER_READ), so a fence-timeout
        // can roll them back to UNDEFINED.
        let mut touched_rts: Vec<u32> = Vec::new();
        let mut videoout_drawn = false;
        // Whether a videoout pass has already run in THIS submit. The first videoout pass uses
        // the shared CLEAR target; every later one LOADs (preserves) so a multi-draw frame
        // accumulates instead of only the last draw surviving (task-149).
        let mut videoout_seen = false;
        // Whether the frame was ALREADY cleared by an earlier submit when this one began. If
        // so, this submit only LOADs; a timeout here must NOT roll the latch back (the earlier
        // clear's content is valid and re-clearing would erase it). Only a submit that itself
        // performed the frame's initial clear needs the rollback.
        let videoout_cleared_at_entry = self.videoout_cleared_this_frame;

        // SAFETY: display thread owns the device; every handle below is created here and
        // destroyed after the fence wait; the target/pipelines/fence are live for the run.
        unsafe {
            let alloc = vk::CommandBufferAllocateInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_ALLOCATE_INFO,
                command_pool: self.ctx.command_pool,
                level: vk::CommandBufferLevel::PRIMARY,
                command_buffer_count: 1,
                ..Default::default()
            };
            let cb = self.ctx.device.allocate_command_buffers(&alloc).unwrap()[0];
            let begin = vk::CommandBufferBeginInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_BEGIN_INFO,
                flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
                ..Default::default()
            };
            self.ctx.device.begin_command_buffer(cb, &begin).unwrap();

            // The per-submit descriptor pools (one per pass that binds descriptors), freed
            // after the fence wait like the command buffer.
            let mut desc_pools: Vec<vk::DescriptorPool> = Vec::new();

            // Time spent creating the transient render passes / framebuffers / descriptor
            // pools, accumulated across the pass loop and folded into `record_ns` (the
            // creation happens interleaved with the recording).
            let mut create_ns = 0u64;
            let mut recorded_draws = 0u64;
            let t_record = prof.then(Instant::now);
            let _record_span = tracing::debug_span!("record").entered();

            for pass in passes {
                match pass.target {
                    PassTarget::Videoout => {
                        let Some(target) = self.draw_target.as_ref() else {
                            continue;
                        };
                        let extent = vk::Extent2D {
                            width: RES_W,
                            height: RES_H,
                        };
                        let (render_pass, framebuffer) =
                            if videoout_seen || self.videoout_cleared_this_frame {
                                // LOAD (preserve) the accumulated frame instead of clearing it, in
                                // EITHER of two cases (both leave the image in SHADER_READ_ONLY):
                                //   * a second+ videoout draw IN THIS submit (task-149), OR
                                //   * the first videoout pass of a LATER submit in the SAME guest
                                //     frame — the frame was already cleared by an earlier submit
                                //     (`videoout_cleared_this_frame`), so re-clearing here would
                                //     erase that submit's geometry (task-152). Celeste splits its
                                //     ~499 videoout draws across many submits per frame; without
                                //     this only the last videoout submit would survive.
                                // The prior videoout pass (this submit or the last) left the image
                                // in SHADER_READ_ONLY_OPTIMAL (its render pass's final layout);
                                // barrier its color writes to be visible to this LOAD pass's color
                                // read/write before the render pass re-transitions it to
                                // COLOR_ATTACHMENT.
                                let barrier = vk::ImageMemoryBarrier {
                                    s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
                                    old_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                                    new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                                    src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                                    dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                                    image: self.ctx.texture_image,
                                    subresource_range: vk::ImageSubresourceRange {
                                        aspect_mask: vk::ImageAspectFlags::COLOR,
                                        base_mip_level: 0,
                                        level_count: 1,
                                        base_array_layer: 0,
                                        layer_count: 1,
                                    },
                                    src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                                    dst_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_READ
                                        | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                                    ..Default::default()
                                };
                                self.ctx.device.cmd_pipeline_barrier(
                                    cb,
                                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                                    vk::DependencyFlags::empty(),
                                    &[],
                                    &[],
                                    &[barrier],
                                );
                                let tc = prof.then(Instant::now);
                                let (rp, fb) = create_videoout_load_target(&self.ctx);
                                if let Some(tc) = tc {
                                    create_ns += tc.elapsed().as_nanos() as u64;
                                }
                                transient_rp.push(rp);
                                transient_fb.push(fb);
                                (rp, fb)
                            } else {
                                (target.render_pass, target.framebuffer)
                            };
                        let (pool, pool_ns) = record_pass_into(
                            &self.ctx,
                            cb,
                            render_pass,
                            framebuffer,
                            extent,
                            pass,
                            prof,
                        );
                        create_ns += pool_ns;
                        if let Some(p) = pool {
                            desc_pools.push(p);
                        }
                        recorded_draws += 1;
                        videoout_drawn = true;
                        videoout_seen = true;
                        // Frame-scoped latch (task-152): once the first videoout pass of the
                        // frame has cleared, every later videoout pass — this submit or a later
                        // one — LOADs and accumulates. Reset at the guest flip (`submit_flip`).
                        self.videoout_cleared_this_frame = true;
                    }
                    PassTarget::RenderTarget(rt_id) => {
                        // Resolve the RT. An unknown id (SetRenderTarget named an RT that was
                        // never created) is skipped — the producer draw is dropped rather than
                        // recorded into an absent target.
                        let Some((image, view, extent, current_layout)) = self
                            .render_targets
                            .get(&rt_id)
                            .map(|rt| (rt.image, rt.view, rt.extent, rt.current_layout))
                        else {
                            continue;
                        };
                        // First use (UNDEFINED) clears; a reuse LOADs whatever the RT already
                        // holds (SHADER_READ from the prior submit) so cross-frame content is
                        // preserved. A wrong initial layout on reuse is a validation fault, so
                        // it MUST equal the tracked `current_layout`.
                        let first_use = current_layout == vk::ImageLayout::UNDEFINED;
                        let tc = prof.then(Instant::now);
                        let (render_pass, framebuffer) =
                            create_rt_target(&self.ctx, view, extent, current_layout, first_use);
                        if let Some(tc) = tc {
                            create_ns += tc.elapsed().as_nanos() as u64;
                        }
                        transient_rp.push(render_pass);
                        transient_fb.push(framebuffer);
                        let (pool, pool_ns) = record_pass_into(
                            &self.ctx,
                            cb,
                            render_pass,
                            framebuffer,
                            extent,
                            pass,
                            prof,
                        );
                        create_ns += pool_ns;
                        if let Some(p) = pool {
                            desc_pools.push(p);
                        }
                        recorded_draws += 1;
                        // The render pass left the RT in COLOR_ATTACHMENT_OPTIMAL (its final
                        // layout); barrier it to SHADER_READ so a later pass samples it.
                        let barrier = vk::ImageMemoryBarrier {
                            s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
                            old_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                            new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                            dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                            image,
                            subresource_range: vk::ImageSubresourceRange {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                base_mip_level: 0,
                                level_count: 1,
                                base_array_layer: 0,
                                layer_count: 1,
                            },
                            src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                            dst_access_mask: vk::AccessFlags::SHADER_READ,
                            ..Default::default()
                        };
                        self.ctx.device.cmd_pipeline_barrier(
                            cb,
                            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                            vk::PipelineStageFlags::FRAGMENT_SHADER,
                            vk::DependencyFlags::empty(),
                            &[],
                            &[],
                            &[barrier],
                        );
                        // The RT is now SHADER_READ for the rest of this submit and the next.
                        if let Some(rt) = self.render_targets.get_mut(&rt_id) {
                            rt.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                        }
                        touched_rts.push(rt_id);
                    }
                }

                // task-179 knob (`UNEMUPS4_X_FULL_BARRIER=1`, default off): a maximally
                // conservative memory barrier after EVERY pass. Discriminates the two
                // candidate causes of the bloom chain reading nothing from the scene RT: if
                // the blurred scene appears with this on, our per-pass barriers are too narrow
                // (a sync bug); if the picture is unchanged, the draw is binding the wrong
                // image and no amount of synchronisation will help.
                static FULL_BARRIER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if *FULL_BARRIER.get_or_init(|| {
                    std::env::var("UNEMUPS4_X_FULL_BARRIER").is_ok_and(|v| v != "0")
                }) {
                    let mem = vk::MemoryBarrier {
                        s_type: vk::StructureType::MEMORY_BARRIER,
                        src_access_mask: vk::AccessFlags::MEMORY_WRITE,
                        dst_access_mask: vk::AccessFlags::MEMORY_READ
                            | vk::AccessFlags::MEMORY_WRITE,
                        ..Default::default()
                    };
                    self.ctx.device.cmd_pipeline_barrier(
                        cb,
                        vk::PipelineStageFlags::ALL_COMMANDS,
                        vk::PipelineStageFlags::ALL_COMMANDS,
                        vk::DependencyFlags::empty(),
                        &[mem],
                        &[],
                        &[],
                    );
                }
            }

            self.ctx.device.end_command_buffer(cb).unwrap();
            if let Some(t) = t_record {
                SUBMIT
                    .record_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            drop(_record_span);

            let cbs = [cb];
            let submit = vk::SubmitInfo {
                s_type: vk::StructureType::SUBMIT_INFO,
                command_buffer_count: 1,
                p_command_buffers: cbs.as_ptr(),
                ..Default::default()
            };
            let fences = [fence];
            let t_submit = prof.then(Instant::now);
            let _submit_span = tracing::debug_span!("queue_submit").entered();
            self.ctx.device.reset_fences(&fences).unwrap();
            self.ctx
                .device
                .queue_submit(self.ctx.queue, &[submit], fence)
                .unwrap();
            if let Some(t) = t_submit {
                SUBMIT
                    .queue_submit_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            drop(_submit_span);

            // Bounded wait so a hung/faulted submit cannot deadlock the display thread.
            let t_fence = prof.then(Instant::now);
            let _fence_span = tracing::debug_span!("draw_fence").entered();
            let waited = self
                .ctx
                .device
                .wait_for_fences(&fences, true, DRAW_FENCE_TIMEOUT_NS);
            if let Some(t) = t_fence {
                SUBMIT
                    .draw_fence_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            drop(_fence_span);
            if waited.is_err() {
                // The submit did not complete (timeout). The RTs' layouts may not actually
                // be SHADER_READ on the device, so roll their tracked `current_layout` back
                // to UNDEFINED — the next submit then clears rather than LOADing from a layout
                // the GPU never reached, which would be a desync validation fault.
                for id in &touched_rts {
                    if let Some(rt) = self.render_targets.get_mut(id) {
                        rt.current_layout = vk::ImageLayout::UNDEFINED;
                    }
                }
                // If THIS submit performed the frame's initial videoout CLEAR (a videoout pass
                // ran and the frame was not already cleared on entry), that clear did not
                // execute — `texture_image` was never cleared. Roll the frame-scoped latch
                // back too (mirroring the RT rollback) so the next submit's first videoout
                // pass CLEARs rather than LOADing onto the previous frame's stale pixels
                // (task-152 latch desync). A submit that only LOADed (frame already cleared by
                // an earlier successful submit) leaves the latch set — its content is valid.
                if videoout_drawn && !videoout_cleared_at_entry {
                    self.videoout_cleared_this_frame = false;
                }
                // The submit is STILL PENDING on the queue (a >5s draw is a fault path, not a
                // completed one), so its command buffer and the transient render passes /
                // framebuffers / descriptor pools it references are in use by the device.
                // Freeing/destroying them now is undefined behavior
                // (VUID-vkFreeCommandBuffers-pCommandBuffers-00047,
                // VUID-vkDestroyRenderPass-renderPass-00873,
                // VUID-vkDestroyFramebuffer-framebuffer-00892,
                // VUID-vkDestroyDescriptorPool-descriptorPool-00303). Leak them instead — the
                // crate's leak-on-exit convention — rather than tear down objects the GPU may
                // still read. Also drop the reused draw fence: the next list must not
                // reset_fences a fence still associated with this pending submit
                // (VUID-vkResetFences-pFences-01123). `ensure_draw_fence` mints a fresh one;
                // this one is left to signal (and leak) on its own.
                self.draw_fence = None;
            } else {
                let t_destroy = prof.then(Instant::now);
                let _destroy_span = tracing::debug_span!("transient_destroy").entered();
                let (n_rp, n_fb, n_pool) = (
                    transient_rp.len() as u64,
                    transient_fb.len() as u64,
                    desc_pools.len() as u64,
                );
                self.ctx
                    .device
                    .free_command_buffers(self.ctx.command_pool, &cbs);
                for rp in transient_rp {
                    self.ctx.device.destroy_render_pass(rp, None);
                }
                for fb in transient_fb {
                    self.ctx.device.destroy_framebuffer(fb, None);
                }
                for pool in desc_pools {
                    self.ctx.device.destroy_descriptor_pool(pool, None);
                }
                if let Some(t) = t_destroy {
                    SUBMIT
                        .transient_destroy_ns
                        .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    SUBMIT
                        .transient_create_ns
                        .fetch_add(create_ns, Ordering::Relaxed);
                    SUBMIT
                        .transient_render_passes
                        .fetch_add(n_rp, Ordering::Relaxed);
                    SUBMIT
                        .transient_framebuffers
                        .fetch_add(n_fb, Ordering::Relaxed);
                    SUBMIT.descriptor_pools.fetch_add(n_pool, Ordering::Relaxed);
                    SUBMIT
                        .passes
                        .fetch_add(passes.len() as u64, Ordering::Relaxed);
                    SUBMIT.draws.fetch_add(recorded_draws, Ordering::Relaxed);
                }
                drop(_destroy_span);
            }
        }
        if let Some(t) = t_passes {
            SUBMIT
                .record_passes_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        videoout_drawn
    }

    /// The per-list draw fence, created lazily and reused. Waiting on this instead of a
    /// global `device_wait_idle` is what lets one submit's whole draw list be a single
    /// GPU wait (doc-2 §3), not O(draws) stalls.
    fn ensure_draw_fence(&mut self) -> vk::Fence {
        if let Some(f) = self.draw_fence {
            return f;
        }
        // SAFETY: display thread owns the device.
        let f = unsafe {
            let info = vk::FenceCreateInfo {
                s_type: vk::StructureType::FENCE_CREATE_INFO,
                ..Default::default()
            };
            self.ctx.device.create_fence(&info, None).unwrap()
        };
        self.draw_fence = Some(f);
        f
    }

    /// Execute an `ImportBuffer` command (doc-2 §8.2). The guest-side `ImportProbe`
    /// already decided this range must import zero-copy, so this is authoritative: the
    /// backend resolves the identity-mapped host pointer and imports under the guest-
    /// minted `id`. A range the guest side promised but the device cannot import is a
    /// **fatal invariant violation** (this panics), never a silent copy fallback — the
    /// cache has already recorded the entry imported (and so clean forever), so falling
    /// back to a copy buffer would strand it holding stale bytes, and every subsequent
    /// draw would consume an absent resource with no retry. A probe-yes the device cannot
    /// honor is a programming error in the guest-side probe, which must only promise
    /// imports the boot-resolved device caps make certain; crashing loud here is strictly
    /// safer than silently serving a stale/absent buffer forever.
    fn replay_import(&mut self, id: ResourceId, addr: u64, size: u64) {
        // SAFETY: `get_host_ptr` resolves the identity-mapped guest range to its stable
        // host pointer; the identity mapping lives for the whole run, satisfying
        // `try_import_host_range`'s validity contract.
        let host_ptr = self
            .guest_memory
            .read()
            .ok()
            .and_then(|mem| unsafe { mem.get_host_ptr(addr) });
        let imported = match host_ptr {
            // SAFETY: `p` names `size` identity-mapped bytes live for the whole run.
            Some(p) => unsafe { self.try_import_host_range(id, p as *const u8, size) },
            None => false,
        };
        assert!(
            imported,
            "zero-copy import promised by the guest-side ImportProbe could not be honored \
             by the device (id={}, addr={addr:#x}, size={size}); the cache has already \
             recorded this entry imported+clean, so there is no correct recovery here — \
             the probe over-promised and must be made conservative",
            id.0
        );
    }

    /// Execute a `FreeResource` command (doc-2 §8): the guest freed/unmapped the range this
    /// resource backed, so destroy its vk allocation — or, for a zero-copy import, revoke
    /// the external-memory buffer so it stops reading the now-freed host pages. The
    /// guest-side cache has already dropped the entry, so this is authoritative teardown.
    ///
    /// **Fence-safe, two ways.** An image, render target or zero-copy import is destroyed
    /// here and now, behind a wait on the in-flight draw fence (created lazily by
    /// `run_command_list`; `None` before the first draw) so no GPU read is live over it.
    /// That is a coarse whole-list wait, acceptable because those frees are rare (guest
    /// teardown), not a per-draw hot path.
    ///
    /// A **cache buffer** takes the other route: it goes on `pending_recycle` and is
    /// returned to the pool at the start of the NEXT command list, with no wait at all
    /// (task-223). It has to be deferred — an earlier draw in the list being walked right
    /// now may bind it, and that draw is not submitted until the walk ends — and it must
    /// not stall, because with the guest-side cache evicting stale entries this is no
    /// longer a rare teardown path but a per-flip one. The next list begins only after this
    /// one was submitted and its fence waited on inside `record_passes`, which is exactly
    /// the guarantee the immediate wait below provides, moved one list later.
    ///
    /// An id present in neither the copy-buffer map (`resources`) nor the import map
    /// (`imported` under the `(-1, id)` cache-facing key) is a no-op: the guest side may
    /// evict an entry the backend never materialized (e.g. one whose create was deferred),
    /// or free a range twice.
    fn free_resource(&mut self, id: ResourceId) {
        let cache_buffer = self.resources.remove(&id.0);
        let imported = self.imported.remove(&(-1, id.0));
        // task-56 step 3: an id may name an offscreen render target instead of a buffer/import
        // (the guest freed the range an RT-as-texture entry backed). Free it here too, fence-safe.
        let render_target = self.render_targets.remove(&id.0);
        // Drop the once-per-target readback refusal with the target itself (task-181): ids are
        // reminted, so keeping it would silence a genuine refusal for whatever surface takes
        // this id next.
        self.readback_refused.remove(&id.0);
        if let Some(buf) = cache_buffer {
            self.pending_recycle.push(buf.pooled);
            if present_profile::enabled() {
                present_profile::POOL.live_buffers.store(
                    self.resources.len() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
        if imported.is_none() && render_target.is_none() {
            return;
        }
        // Wait on the in-flight draw list before freeing so no GPU read is live over the
        // resource (fence-safe teardown). `draw_fence` is `None` until the first draw list;
        // if unset there is no in-flight GPU work referencing this resource.
        if let Some(fence) = self.draw_fence {
            // SAFETY: display thread owns the device; the fence is the live per-list fence.
            // Bounded like every other draw-fence wait (DRAW_FENCE_TIMEOUT_NS): a prior list
            // whose submit hung leaves this fence never-signaled, and an unbounded u64::MAX
            // wait here would deadlock the display thread. On timeout proceed with teardown —
            // the same fault path the other bounded waits take.
            unsafe {
                self.ctx
                    .device
                    .wait_for_fences(&[fence], true, DRAW_FENCE_TIMEOUT_NS)
                    .ok();
            }
        }
        // SAFETY: the wait above guarantees no in-flight GPU work references these; the
        // buffers/memory were allocated on this thread and are not aliased elsewhere.
        unsafe {
            if let Some(imp) = imported {
                self.ctx.destroy_imported_buffer(&imp);
            }
            if let Some(rt) = render_target {
                self.ctx.device.destroy_image_view(rt.view, None);
                self.ctx.device.destroy_image(rt.image, None);
                self.ctx.device.free_memory(rt.mem, None);
            }
        }
    }

    /// Build the host pipeline a `CreatePipeline` command names and record it under its
    /// guest-minted id (doc-2 §4, decision-7). The shared render target (render pass +
    /// framebuffer over the videoout `texture_image`) is built on first use. A re-create
    /// under an id already present is a no-op — the guest-side cache emits `CreatePipeline`
    /// only once per pipeline, so this is defensive, not the steady-state path.
    ///
    /// `key`/`target` carry the pipeline-state the build keys on; this phase renders into
    /// the fixed videoout target (no MRT/depth), so the fields are threaded through without
    /// yet consuming blend/depth/vertex-layout — those grow one-per-milestone with the state
    /// model. The SPIR-V words are the sole build input today.
    #[allow(clippy::too_many_arguments)]
    fn create_pipeline(
        &mut self,
        id: PipelineId,
        vs_spirv: &[u32],
        ps_spirv: &[u32],
        key: &PipelineKey,
        target: &TargetDesc,
        vertex_storage: &[StorageBinding],
        push_constants: Option<PushConstantRange>,
        textures: &[TextureBinding],
        const_storage: Option<StorageBinding>,
        const_storage_fragment: Option<StorageBinding>,
    ) {
        let _ = target;
        if self.pipelines.contains_key(&id.0) {
            return;
        }
        // Lazily build the render pass + framebuffer over the videoout texture_image.
        if self.draw_target.is_none() {
            // SAFETY: device owned by this thread; texture_image/texture_view are live.
            let created = unsafe { create_embedded_target(&self.ctx) };
            self.draw_target = Some(created);
        }
        let Some(render_pass) = self.draw_target.as_ref().map(|t| t.render_pass) else {
            return;
        };

        // SAFETY: same device-ownership contract; SPIR-V words are valid modules
        // (spirv-val-gated upstream, decision-3). Built inside the portability subset.
        // `vertex_storage` selects the layout: non-empty builds one STORAGE_BUFFER binding
        // PER V# stream (task-153) + a push-constant range spanning all stream groups, with
        // EMPTY vertex input (a recompiled VS fetches via SSBOs + gl_VertexIndex); empty uses
        // `key.vertex_layout` for the vertex-input state (None there = the embedded
        // gl_VertexIndex path, an empty pipeline layout).
        let built = unsafe {
            create_host_pipeline(
                &self.ctx,
                render_pass,
                vs_spirv,
                ps_spirv,
                key.vertex_layout,
                vertex_storage,
                push_constants,
                textures,
                const_storage,
                const_storage_fragment,
                key.blend,
                key.topology,
            )
        };
        self.pipelines.insert(
            id.0,
            HostPipeline {
                needs_const: u32::from(const_storage.is_some())
                    + u32::from(const_storage_fragment.is_some()),
                needs_storage: vertex_storage.len() as u32,
                needs_texture: textures.len() as u32,
                ..built
            },
        );
    }

    /// Create a sampled image under its guest-minted id (doc-2 §C3/§C4). Allocates the
    /// device-local `vk::Image` + view + memory and records `id -> image`; the pixels arrive
    /// separately via [`Self::upload_image`]. A re-create under an id already present is a
    /// no-op — the cache emits one create per image.
    fn create_image(&mut self, id: ResourceId, width: u32, height: u32, format: TextureFormat) {
        if self.images.contains_key(&id.0) {
            return;
        }
        let vk_format = vk_texture_format(format);
        // SAFETY: display thread owns the device; ctx is live.
        let (image, view, mem) = match unsafe {
            self.ctx.create_sampled_image(width, height, vk_format)
        } {
            Ok(t) => t,
            Err(e) => {
                // A degenerate/oversized guest texture descriptor the device cannot
                // allocate: skip the resource. A later BindTexture finds no image and the
                // draw is dropped by the cache-miss guard, degrading rather than aborting.
                tracing::warn!(
                    "create_image {}: sampled image {width}x{height} allocation failed: {e}; skipping",
                    id.0
                );
                return;
            }
        };
        self.images.insert(
            id.0,
            CacheImage {
                image,
                view,
                mem,
                extent: vk::Extent2D { width, height },
            },
        );
    }

    /// Create an offscreen render target under its guest-minted id (doc-2 §8.5, task-56
    /// RT-as-texture). Allocates one device-local `vk::Image` + view + memory usable as BOTH
    /// a color attachment and a sampled source (mirrors [`Self::create_image`]; the
    /// one-image-both-roles pattern proven portable by the videoout `texture_image`). Initial
    /// layout is UNDEFINED — the RT is not rendered into here (that is the step-4 multi-pass
    /// refactor); this step only makes the RT image EXIST so a later `BindTexture` can sample
    /// it. A re-create under an id already present is a no-op — the cache emits one
    /// `CreateRenderTarget` per RT.
    fn create_render_target(
        &mut self,
        id: ResourceId,
        width: u32,
        height: u32,
        format: ColorFormat,
    ) {
        if self.render_targets.contains_key(&id.0) {
            return;
        }
        let vk_format = vk_color_format(format);
        // SAFETY: display thread owns the device; ctx is live.
        let (image, view, mem) = match unsafe {
            self.ctx
                .create_render_target_image(width, height, vk_format)
        } {
            Ok(t) => t,
            Err(e) => {
                // A degenerate/oversized guest render-target descriptor the device cannot
                // allocate: skip it. A later pass targeting this id finds no RT and is skipped,
                // degrading rather than aborting.
                tracing::warn!(
                    "create_render_target {}: image {width}x{height} allocation failed: {e}; skipping",
                    id.0
                );
                return;
            }
        };
        self.render_targets.insert(
            id.0,
            CacheRenderTarget {
                image,
                view,
                mem,
                extent: vk::Extent2D { width, height },
                // Created UNDEFINED; the step-4 write pass transitions + updates this.
                current_layout: vk::ImageLayout::UNDEFINED,
            },
        );
    }

    /// Stage detiled linear RGBA `pixels` into the sampled image `id` and leave it
    /// `SHADER_READ_ONLY_OPTIMAL` (doc-2 §C3). Unknown ids (never created) are a no-op.
    fn upload_image(&mut self, id: ResourceId, pixels: &[u8]) {
        if let Some(img) = self.images.get(&id.0) {
            let (image, w, h) = (img.image, img.extent.width, img.extent.height);
            // SAFETY: `image` is the sampled image created under this id; ctx is live.
            unsafe { self.ctx.upload_image(image, w, h, pixels) };
        }
    }

    /// Create a sampler under its guest-minted id from the portable [`SamplerDesc`]
    /// (doc-2 §C4). A re-create under an id already present is a no-op.
    fn create_sampler(&mut self, id: ResourceId, desc: SamplerDesc) {
        if self.samplers.contains_key(&id.0) {
            return;
        }
        // SAFETY: display thread owns the device; ctx is live.
        let sampler = unsafe {
            self.ctx.create_sampler(
                vk_filter(desc.mag_filter),
                vk_filter(desc.min_filter),
                vk_address_mode(desc.address_mode_u),
                vk_address_mode(desc.address_mode_v),
            )
        };
        self.samplers.insert(id.0, sampler);
    }

    /// Registers a guest framebuffer and, when eligible, imports it zero-copy.
    /// Relocated verbatim from the `RegisterBuffer` display-loop arm.
    pub fn register_buffer(
        &mut self,
        ptr: u64,
        w: u32,
        h: u32,
        // The guest scanout pixelFormat, stored so the present path can decode the R↔B
        // swap / sRGB flag it drives into the present shader (task-154 residual #2).
        pixel_format: u32,
        handle: i32,
        index: u32,
    ) {
        let key = (handle, index);
        self.buffers.insert(
            key,
            DisplayBuffer {
                guest_ptr: ptr,
                width: w,
                height: h,
                pixel_format,
            },
        );

        // Zero-copy import: resolve the stable host pointer for this guest
        // framebuffer and, when the extension is enabled and the pointer is
        // alignable, import it once so the flip path can skip the memcpy. Any
        // failure leaves no cache entry -> staging fallback. Only import full-frame
        // buffers: the per-frame GPU copy region is fixed at RES_W x RES_H, so an
        // imported buffer MUST cover at least that many bytes or the copy would read
        // out of bounds of the guest's own pages. The staging path handles smaller
        // buffers by clamping the memcpy; the zero-copy path cannot, so it declines
        // here and falls back to staging for non-full-frame buffers.
        let full_frame = w == RES_W && h == RES_H;
        // SAFETY: `get_host_ptr` resolves an identity-mapped guest address to its
        // stable host pointer; `ptr` is the guest framebuffer address just registered.
        let host_ptr_opt = self
            .guest_memory
            .read()
            .ok()
            .and_then(|mem| unsafe { mem.get_host_ptr(ptr) });
        if full_frame
            && self.ctx.ext_mem_host.is_some()
            && let Some(host_ptr) = host_ptr_opt
        {
            let host_ptr = host_ptr as *const u8;
            let size = (w * h * 4) as u64;

            // If this key was previously imported at a different host pointer, free
            // the stale import before re-importing (avoids unbounded growth on
            // re-register). Safe: no GPU work references it — the fence for the last
            // submit is waited on each frame, and register runs between frames.
            let stale = self
                .imported
                .get(&key)
                .is_some_and(|b| b.host_ptr != host_ptr);
            if stale && let Some(old) = self.imported.remove(&key) {
                unsafe {
                    self.ctx.device.device_wait_idle().ok();
                    self.ctx.destroy_imported_buffer(&old);
                }
            }

            if !self.imported.contains_key(&key) {
                // SAFETY: `host_ptr` is the identity-mapped guest framebuffer, valid
                // for the whole run; the import is dropped before that memory is.
                if let Some(buf) = unsafe { self.try_import(host_ptr, size) } {
                    self.imported.insert(key, buf);
                }
            }
        }
    }

    /// Records a flip target and its pending vsync signal, returning the sentinel
    /// [`TargetId`] to hand to [`GpuBackend::present`]. Relocated from the
    /// `SubmitFlip` display-loop arm.
    pub fn submit_flip(&mut self, handle: i32, index: u32, signal: Sender<()>) -> TargetId {
        // A guest flip ends the current frame (task-152): reset the videoout clear latch so the
        // NEXT frame's first videoout submit clears the target fresh, while all of THIS frame's
        // submits accumulated (the first cleared, the rest LOADed). Present is decoupled (the
        // display loop presents continuously), so the flip — not present — is the frame boundary.
        self.videoout_cleared_this_frame = false;
        self.current_target = Some((handle, index));
        if let Some(old_signal) = self.pending_vsync_signal.replace(signal) {
            let _ = old_signal.send(());
        }
        CURRENT_TARGET
    }

    /// Signals the pending guest vsync channel, if any.
    pub fn signal_vsync(&mut self) {
        if let Some(tx) = self.pending_vsync_signal.take() {
            let _ = tx.send(());
        }
    }

    /// Immutable access to the underlying context (device idle on close, etc.).
    pub fn ctx(&self) -> &VulkanContext {
        &self.ctx
    }

    /// The device capabilities queried at selection (task-136). The seam a future
    /// caps-tiered Vulkan/MoltenVK path would consult (e.g. swapchain image-count
    /// sizing, SPIR-V feature clamping). Read-only; nothing gates on it yet.
    pub fn caps(&self) -> ps4_core::gpu::GpuCaps {
        self.caps
    }

    /// The lower-level import used by both `register_buffer` and
    /// `try_import_host_range`. Wraps `VulkanContext::try_import_host_buffer`.
    ///
    /// # Safety
    /// See [`VulkanContext::try_import_host_buffer`].
    unsafe fn try_import(&self, host_ptr: *const u8, size: u64) -> Option<ImportedBuf> {
        unsafe { self.ctx.try_import_host_buffer(host_ptr, size) }
    }
}

impl GpuBackend for AshBackend {
    fn present(&mut self, target: TargetId) -> Result<(), GpuError> {
        debug_assert_eq!(target, CURRENT_TARGET);
        let _ = target;
        let prof = self.prof;
        // Phase 3.5: when a draw has populated `texture_image`, skip the guest-framebuffer
        // buffer→image copy so the present blit scans out the drawn pixels. LATCHED (read,
        // not consumed) so a scene drawn once stays on screen across the display loop's
        // continuous presents instead of flashing for a single vsync.
        let embedded_drawn = self.embedded_drawn;

        // Decode the present-shader R↔B swap from the flipped buffer's guest scanout
        // pixelFormat (task-154 residual #2): a BGRA scanout (A8R8G8B8_SRGB, e.g. Celeste)
        // needs the swap; an RGBA one (A8B8G8R8_SRGB, the example) does not. If no flip
        // target is registered yet (e.g. an embedded-only draw), fall back to the PS4 default
        // scanout format (A8R8G8B8_SRGB → swap), matching the videoout default.
        //
        // The swap describes the BYTE ORDER OF GUEST MEMORY, so it applies ONLY when the
        // present actually sources pixels from the guest framebuffer (task-175). An embedded
        // draw does not: our render pass wrote shader-space (r,g,b,a) straight into the RGBA
        // `texture_image`, so the channels are already in place. On real hardware the guest's
        // `CB_COLOR0_INFO.COMP_SWAP` and the scanout pixelFormat describe the same buffer and
        // compose to the identity — Celeste programs COMP_SWAP=ALT with an A8R8G8B8 scanout,
        // and its red still reaches the display as red. Applying the scanout swap on top of an
        // embedded draw was therefore a pure, unmatched R↔B flip.
        let scanout_pixel_format = self
            .current_target
            .and_then(|key| self.buffers.get(&key))
            .map(|buf| buf.pixel_format)
            .unwrap_or(0x8000_0000);
        let swap_rb = u32::from(!embedded_drawn && scanout_swap_rb(scanout_pixel_format));
        // `texture_image` is _UNORM (task-175), so what the present samples is the guest's
        // own GAMMA-SPACE value, byte for byte. An _SRGB swapchain encodes linear→sRGB on
        // store, which would corrupt that value, so the shader DECODES sRGB→linear first and
        // the two cancel. A non-_SRGB swapchain stores raw and needs no correction.
        // `decode_srgb = 1` selects the in-shader decode.
        let decode_srgb = u32::from(format_is_srgb(self.ctx.swapchain_format));

        // Phase 1 (immutable ctx borrow): fence wait -> acquire -> fb copy/import.
        // Produces the Copy-only values `image_index`, `src_buffer`, `zero_copy_key`
        // consumed by the later phases; the borrow ends before the first mutable
        // `signal_vsync`. The `?`/early-return arms below match the original exactly.
        let (image_index, src_buffer, zero_copy_key) = unsafe {
            let ctx = &self.ctx;
            // wait on fence
            let fences = [ctx.in_flight_fence];
            let _g = tracing::debug_span!("fence_wait").entered();
            let t = prof.then(Instant::now);
            ctx.device
                .wait_for_fences(&fences, true, u64::MAX)
                .map_err(|e| GpuError::Present(format!("wait_for_fences: {e}")))?;
            if let Some(t) = t {
                PRESENT
                    .fence_wait_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            drop(_g);

            // acquire next image
            let _g = tracing::debug_span!("acquire").entered();
            let t = prof.then(Instant::now);
            let (image_index, _is_suboptimal) = match ctx.swapchain_loader.acquire_next_image(
                ctx.swapchain,
                u64::MAX,
                ctx.image_available_semaphore,
                vk::Fence::null(),
            ) {
                Ok(res) => res,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    return Ok(());
                }
                Err(e) => {
                    tracing::error!("Swapchain acquire failed: {}", e);
                    return Ok(());
                }
            };
            if let Some(t) = t {
                PRESENT
                    .acquire_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            drop(_g);

            ctx.device.reset_fences(&fences).unwrap();

            // Select the buffer->image copy source for this flip.
            //  - Zero-copy path: if the current target was imported via
            //    VK_EXT_external_memory_host, the GPU reads the guest framebuffer
            //    directly; NO memcpy happens.
            //  - Staging path: memcpy the guest framebuffer into the mapped staging
            //    buffer, exactly as before.
            let _g = tracing::debug_span!("fb_copy").entered();
            let t = prof.then(Instant::now);
            // Phase 3.5: an embedded draw already wrote texture_image; there is no
            // guest framebuffer to copy, and doing so would overwrite the draw. Treat
            // it as a non-imported flip whose copy is skipped by record_command_buffer.
            let zero_copy_key = if embedded_drawn {
                None
            } else {
                self.current_target
                    .filter(|key| self.imported.contains_key(key))
            };
            let src_buffer = if let Some(key) = zero_copy_key {
                // Imported buffer: skip the memcpy entirely.
                self.imported[&key].buffer
            } else {
                if !embedded_drawn
                    && let (Some(key), Ok(mem)) = (self.current_target, self.guest_memory.read())
                    && let Some(buf) = self.buffers.get(&key)
                    && let Some(guest_ptr) = mem.get_host_ptr(buf.guest_ptr)
                {
                    // width/height are guest-controlled (sceVideoOutRegisterBuffers accepts
                    // any non-zero values), so a u32*u32*4 multiply can wrap and yield a small
                    // in-range length that reads far past the source allocation. Compute in
                    // usize with saturating math; the `.min(max_size)` clamp below then bounds
                    // the copy to the staging buffer regardless.
                    let copy_size = (buf.width as usize)
                        .saturating_mul(buf.height as usize)
                        .saturating_mul(4);
                    let max_size = (RES_W * RES_H * 4) as usize;
                    // Also bound the read by the guest mapping that actually backs `guest_ptr`:
                    // a buffer registered as full-frame but backed by fewer pages must not be
                    // read past its VMA end into adjacent host memory (or an unmapped hole →
                    // SIGSEGV). `query_region` returns the containing VMA on a range-tracking
                    // backend; on a backend that does not track VMAs it returns None and the
                    // `max_size` clamp above remains the only bound.
                    let mapped = mem
                        .query_region(buf.guest_ptr, false)
                        .map(|vma| vma.end.saturating_sub(buf.guest_ptr) as usize)
                        .unwrap_or(max_size);
                    let actual_size = copy_size.min(max_size).min(mapped);

                    std::ptr::copy_nonoverlapping(
                        guest_ptr as *const u8,
                        ctx.staging_ptr,
                        actual_size,
                    );
                }
                ctx.staging_buffer
            };
            if let Some(t) = t {
                PRESENT
                    .fb_copy_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            drop(_g);

            (image_index, src_buffer, zero_copy_key)
        };

        // Guest vsync signal timing.
        //
        // STAGING path: the memcpy above has fully completed and decoupled the guest
        // framebuffer from what the GPU reads, so we signal the guest here
        // (flip-QUEUED, not flip-DISPLAYED). The guest double-buffers and can render
        // its next frame in parallel with the GPU submit/present/pacing below -- this
        // is what lets a >16.6ms serial chain stop quantizing to 2 vsync periods
        // (~34 fps). See TASK-17.
        //
        // ZERO-COPY path: there is NO memcpy to anchor to; the GPU reads the guest's
        // own pages directly during the buffer->image transfer. Signalling here would
        // let the guest overwrite that buffer while the transfer is still in flight.
        // So on this path we DEFER the signal to just after queue_submit (see below):
        // the submit has captured the transfer that reads the imported memory, and
        // combined with guest double-buffering the guest may then safely proceed.
        // Correctness over speed.
        if zero_copy_key.is_none() {
            self.signal_vsync();
        }

        // Phase 2 (immutable ctx borrow): record + submit.
        let t_record = prof.then(Instant::now);
        let _g = tracing::debug_span!("record_submit").entered();
        unsafe {
            let ctx = &self.ctx;
            // record every frame; never submit an empty buffer. When an embedded
            // draw populated texture_image, skip the buffer→image copy and
            // its barriers — the image is already SHADER_READ_ONLY_OPTIMAL — and just
            // sample it into the swapchain.
            record_command_buffer(
                ctx,
                image_index,
                src_buffer,
                embedded_drawn,
                swap_rb,
                decode_srgb,
            );

            // submit
            let wait_semaphores = [ctx.image_available_semaphore];
            let signal_semaphores = [ctx.render_finished_semaphore];
            let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let command_buffers = [ctx.command_buffer];

            let submit_info = vk::SubmitInfo {
                s_type: vk::StructureType::SUBMIT_INFO,
                wait_semaphore_count: 1,
                p_wait_semaphores: wait_semaphores.as_ptr(),
                p_wait_dst_stage_mask: wait_stages.as_ptr(),
                command_buffer_count: 1,
                p_command_buffers: command_buffers.as_ptr(),
                signal_semaphore_count: 1,
                p_signal_semaphores: signal_semaphores.as_ptr(),
                ..Default::default()
            };

            ctx.device
                .queue_submit(ctx.queue, &[submit_info], ctx.in_flight_fence)
                .unwrap();
        }

        // ZERO-COPY path: signal the guest now, after the submit that
        // captured the buffer->image transfer reading the imported guest memory. The
        // guest double-buffers, so it renders into the OTHER buffer next while this
        // transfer completes on the GPU -- no torn read of the imported pages. (On
        // the staging path the guest was already signalled after the memcpy above;
        // this take() is a no-op there.)
        if zero_copy_key.is_some() {
            self.signal_vsync();
        }

        if let Some(t) = t_record {
            PRESENT
                .record_submit_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        drop(_g);

        // Env-gated visual oracle: read the just-composited swapchain image back
        // to an RGBA PNG so the presented frame can be judged headlessly. Done BEFORE
        // queue_present, while the present render pass has left the image in PRESENT_SRC
        // and we still own it (after queue_present the presentation engine owns it and a
        // readback would race). No-op unless UNEMUPS4_DUMP_PNG is set.
        if let Some(dump) = self.dump_png.as_mut() {
            dump_present_png(&self.ctx, dump, image_index);
        }

        // Phase 3 (immutable ctx borrow): present.
        unsafe {
            let ctx = &self.ctx;
            let _g = tracing::debug_span!("present").entered();
            let signal_semaphores = [ctx.render_finished_semaphore];
            let swapchains = [ctx.swapchain];
            let image_indices = [image_index];
            let present_info = vk::PresentInfoKHR {
                s_type: vk::StructureType::PRESENT_INFO_KHR,
                wait_semaphore_count: 1,
                p_wait_semaphores: signal_semaphores.as_ptr(),
                swapchain_count: 1,
                p_swapchains: swapchains.as_ptr(),
                p_image_indices: image_indices.as_ptr(),
                ..Default::default()
            };

            let t = prof.then(Instant::now);
            let _ = ctx.swapchain_loader.queue_present(ctx.queue, &present_info);
            if let Some(t) = t {
                PRESENT
                    .present_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                PRESENT.frames.fetch_add(1, Ordering::Relaxed);
                crate::present_profile::note_present();
            }
            drop(_g);
        }

        Ok(())
    }

    fn create_target(&mut self, desc: &TargetDesc) -> TargetId {
        let _ = desc;
        let id = self.next_id;
        self.next_id += 1;
        TargetId(id)
    }

    fn create_resource(&mut self, id: ResourceId, desc: &ResourceDesc) {
        // Id-ownership (doc-2 §3): `id` is minted guest-side by the
        // ps4-gnm ResourceCache; the backend only allocates VRAM under it and records
        // it in `resources`. Linear host-visible buffer for the phase-3.5 copy path,
        // suballocated out of a shared mapped block (task-223) rather than taking a device
        // allocation and a mapping of its own.
        let len = desc.size.max(1);
        // SAFETY: display thread owns the device; ctx is live.
        let Some(pooled) = (unsafe { self.pool.alloc(&self.ctx, len) }) else {
            // The pool could not back this allocation (an oversized descriptor under memory
            // pressure). Skip the resource: a later upload/bind for this id finds no entry and
            // is a no-op, and draws that need it are dropped by the cache-miss guard — the
            // guest degrades rather than the display thread aborting.
            tracing::warn!(
                "create_resource {}: pool allocation of {len} bytes failed; skipping",
                id.0
            );
            return;
        };
        self.resources.insert(id.0, CacheBuffer { pooled, len });
        if present_profile::enabled() {
            present_profile::POOL.live_buffers.store(
                self.resources.len() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    fn upload(&mut self, id: ResourceId, offset: u64, bytes: &[u8]) {
        // Copy path (doc-2 §8.2): memcpy the guest bytes into the host-coherent cache
        // buffer through its persistent mapping (task-222). Unknown ids (never created)
        // are a no-op — the cache only uploads to ids it created. An upload that would
        // run off the end of the allocation is dropped rather than truncated: the cache
        // sizes the buffer from the same descriptor it uploads against, so a mismatch is
        // a cache bug, and a partial write would silently render wrong geometry.
        let Some(res) = self.resources.get(&id.0) else {
            return;
        };
        if bytes.is_empty() {
            return;
        }
        if offset.saturating_add(bytes.len() as u64) > res.len {
            tracing::warn!(
                "upload to resource {} of {} bytes at offset {} exceeds its {}-byte allocation; skipping",
                id.0,
                bytes.len(),
                offset,
                res.len,
            );
            return;
        }
        // SAFETY: `ptr` maps `len` bytes of a live host-coherent allocation and the range
        // check above keeps `offset + bytes.len()` inside it; `bytes` cannot alias the
        // device mapping. HOST_COHERENT, so the write needs no explicit flush.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                res.ptr().add(offset as usize),
                bytes.len(),
            )
        };
    }

    unsafe fn try_import_host_range(
        &mut self,
        id: ResourceId,
        host_ptr: *const u8,
        size: u64,
    ) -> bool {
        // Zero-copy fork (doc-2 §8.2): import the guest range directly under the
        // caller-supplied guest-minted `id`. The present path caches imports keyed by
        // (handle, index); this cache-facing seam holds the import live in `imported`
        // under a synthetic key so it is not dropped. `false` -> caller falls back to
        // create_resource + upload.
        match unsafe { self.try_import(host_ptr, size) } {
            Some(buf) => {
                self.imported.insert((-1, id.0), buf);
                true
            }
            None => false,
        }
    }
}

/// Records the per-frame command buffer. `src_buffer` is the buffer->image copy
/// source: `ctx.staging_buffer` on the staging path, or an imported guest
/// framebuffer on the zero-copy path. Barriers and copy region are
/// identical for both. When `embedded_drawn`, the buffer→image copy and
/// its barriers are skipped: an embedded draw already wrote `texture_image` and left
/// it `SHADER_READ_ONLY_OPTIMAL`, so this only samples it into the swapchain.
unsafe fn record_command_buffer(
    ctx: &VulkanContext,
    image_index: u32,
    src_buffer: vk::Buffer,
    embedded_drawn: bool,
    // Present-shader push constants (task-154 residual #2, task-175): `swap_rb` R↔B-swaps
    // a BGRA guest scanout, `decode_srgb` undoes an _SRGB swapchain's encode-on-store.
    swap_rb: u32,
    decode_srgb: u32,
) {
    unsafe {
        let cb = ctx.command_buffer;

        ctx.device
            .reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())
            .unwrap();

        let begin_info = vk::CommandBufferBeginInfo {
            s_type: vk::StructureType::COMMAND_BUFFER_BEGIN_INFO,
            flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
            ..Default::default()
        };

        ctx.device.begin_command_buffer(cb, &begin_info).unwrap();

        if !embedded_drawn {
            let barrier_to_transfer = vk::ImageMemoryBarrier {
                s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
                old_layout: vk::ImageLayout::UNDEFINED,
                new_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                image: ctx.texture_image,
                subresource_range: vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                src_access_mask: vk::AccessFlags::empty(),
                dst_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                ..Default::default()
            };

            ctx.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier_to_transfer],
            );

            let region = vk::BufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
                image_extent: vk::Extent3D {
                    width: RES_W,
                    height: RES_H,
                    depth: 1,
                },
            };

            ctx.device.cmd_copy_buffer_to_image(
                cb,
                src_buffer,
                ctx.texture_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );

            let barrier_to_shader = vk::ImageMemoryBarrier {
                s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
                old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                image: ctx.texture_image,
                subresource_range: vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                src_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                dst_access_mask: vk::AccessFlags::SHADER_READ,
                ..Default::default()
            };

            ctx.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier_to_shader],
            );
        } // end if !embedded_drawn

        let clear_values = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.2, 0.4, 0.8, 1.0],
            },
        }];

        let render_pass_begin = vk::RenderPassBeginInfo {
            s_type: vk::StructureType::RENDER_PASS_BEGIN_INFO,
            render_pass: ctx.render_pass,
            framebuffer: ctx.framebuffers[image_index as usize],
            render_area: vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: ctx.swapchain_extent,
            },
            clear_value_count: 1,
            p_clear_values: clear_values.as_ptr(),
            ..Default::default()
        };

        ctx.device
            .cmd_begin_render_pass(cb, &render_pass_begin, vk::SubpassContents::INLINE);

        ctx.device
            .cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, ctx.pipeline);

        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: ctx.swapchain_extent.width as f32,
            height: ctx.swapchain_extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: ctx.swapchain_extent,
        }];
        ctx.device.cmd_set_viewport(cb, 0, &viewports);
        ctx.device.cmd_set_scissor(cb, 0, &scissors);
        ctx.device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            ctx.pipeline_layout,
            0,
            &[ctx.descriptor_set],
            &[],
        );

        // Push the scanout swap/sRGB flags to the present frag shader (task-154 residual
        // #2): two little-endian u32s at offset 0, matching the FRAGMENT push-constant
        // range in the present pipeline layout.
        let mut pc = [0u8; 8];
        pc[0..4].copy_from_slice(&swap_rb.to_le_bytes());
        pc[4..8].copy_from_slice(&decode_srgb.to_le_bytes());
        ctx.device.cmd_push_constants(
            cb,
            ctx.pipeline_layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            &pc,
        );

        ctx.device.cmd_draw(cb, 3, 1, 0, 0);

        ctx.device.cmd_end_render_pass(cb);
        ctx.device.end_command_buffer(cb).unwrap();
    }
}

/// The videoout target format the embedded draw renders into — must match the
/// `texture_image` format (`R8G8B8A8_UNORM`, see vulkan.rs). _UNORM mirrors the guest's
/// `CB_COLOR0_INFO.NUMBER_TYPE`, which is NUMBER_UNORM for every videoout target in the
/// corpus (task-175); an _SRGB attachment encoded the guest's already-gamma-space fragment
/// values a second time and washed the scene out. Blending therefore happens in gamma
/// space, which is what a UNORM CB does on real hardware. Part of the pipeline key on real
/// HW (doc-2 §8); fixed for phase 3.5, so not yet threaded into the cache key.
const EMBEDDED_TARGET_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// Create the render pass + framebuffer that target the videoout `texture_image` for
/// the phase-3.5 embedded draw. The color attachment is CLEARed then STOREd
/// and ends in `SHADER_READ_ONLY_OPTIMAL` so the present blit samples it without an
/// extra transition. All core Vulkan 1.0 — Vulkan-portable subset (decision-3).
///
/// # Safety
/// `ctx.device`/`texture_view` must be live and owned by the calling (display) thread.
unsafe fn create_embedded_target(ctx: &VulkanContext) -> EmbeddedTarget {
    unsafe {
        let attachments = [vk::AttachmentDescription {
            format: EMBEDDED_TARGET_FORMAT,
            samples: vk::SampleCountFlags::TYPE_1,
            load_op: vk::AttachmentLoadOp::CLEAR,
            store_op: vk::AttachmentStoreOp::STORE,
            stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
            stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
            initial_layout: vk::ImageLayout::UNDEFINED,
            final_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            flags: vk::AttachmentDescriptionFlags::empty(),
        }];
        let color_ref = [vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }];
        let subpass = [vk::SubpassDescription {
            pipeline_bind_point: vk::PipelineBindPoint::GRAPHICS,
            color_attachment_count: 1,
            p_color_attachments: color_ref.as_ptr(),
            ..Default::default()
        }];
        let rp_info = vk::RenderPassCreateInfo {
            s_type: vk::StructureType::RENDER_PASS_CREATE_INFO,
            attachment_count: 1,
            p_attachments: attachments.as_ptr(),
            subpass_count: 1,
            p_subpasses: subpass.as_ptr(),
            ..Default::default()
        };
        let render_pass = ctx.device.create_render_pass(&rp_info, None).unwrap();

        let fb_attachments = [ctx.texture_view];
        let fb_info = vk::FramebufferCreateInfo {
            s_type: vk::StructureType::FRAMEBUFFER_CREATE_INFO,
            render_pass,
            attachment_count: 1,
            p_attachments: fb_attachments.as_ptr(),
            width: RES_W,
            height: RES_H,
            layers: 1,
            ..Default::default()
        };
        let framebuffer = ctx.device.create_framebuffer(&fb_info, None).unwrap();

        EmbeddedTarget {
            render_pass,
            framebuffer,
        }
    }
}

/// Build a TRANSIENT render pass + framebuffer over the videoout `texture_view` that LOADs
/// (preserves) the image's existing contents instead of clearing them (task-149). Used for
/// EVERY videoout pass after the first in a submit: Celeste renders a frame as several draws
/// per submit into the videoout target, and if each pass CLEARed (like the shared
/// `EmbeddedTarget`), only the last draw would survive — the frame goes black. The first pass
/// CLEARs (leaving SHADER_READ), so this LOAD pass takes initial=SHADER_READ and ends in
/// SHADER_READ again, so consecutive videoout passes chain and the present blit still reads a
/// SHADER_READ image. Destroyed after the fence wait (pushed into the transient lists).
///
/// # Safety
/// `ctx.device` / `ctx.texture_view` must be live and owned by the calling (display) thread.
unsafe fn create_videoout_load_target(ctx: &VulkanContext) -> (vk::RenderPass, vk::Framebuffer) {
    unsafe {
        let attachments = [vk::AttachmentDescription {
            format: EMBEDDED_TARGET_FORMAT,
            samples: vk::SampleCountFlags::TYPE_1,
            // LOAD preserves the prior videoout pass's pixels; the shared CLEAR target already
            // ran for the first pass, leaving the image in SHADER_READ_ONLY_OPTIMAL.
            load_op: vk::AttachmentLoadOp::LOAD,
            store_op: vk::AttachmentStoreOp::STORE,
            stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
            stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
            initial_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            final_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            flags: vk::AttachmentDescriptionFlags::empty(),
        }];
        let color_ref = [vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }];
        let subpass = [vk::SubpassDescription {
            pipeline_bind_point: vk::PipelineBindPoint::GRAPHICS,
            color_attachment_count: 1,
            p_color_attachments: color_ref.as_ptr(),
            ..Default::default()
        }];
        let rp_info = vk::RenderPassCreateInfo {
            s_type: vk::StructureType::RENDER_PASS_CREATE_INFO,
            attachment_count: 1,
            p_attachments: attachments.as_ptr(),
            subpass_count: 1,
            p_subpasses: subpass.as_ptr(),
            ..Default::default()
        };
        let render_pass = ctx.device.create_render_pass(&rp_info, None).unwrap();

        let fb_attachments = [ctx.texture_view];
        let fb_info = vk::FramebufferCreateInfo {
            s_type: vk::StructureType::FRAMEBUFFER_CREATE_INFO,
            render_pass,
            attachment_count: 1,
            p_attachments: fb_attachments.as_ptr(),
            width: RES_W,
            height: RES_H,
            layers: 1,
            ..Default::default()
        };
        let framebuffer = ctx.device.create_framebuffer(&fb_info, None).unwrap();
        (render_pass, framebuffer)
    }
}

/// Build a graphics pipeline from a VS + PS SPIR-V pair for a host draw. Declares
/// dynamic viewport + scissor (`VK_DYNAMIC_STATE_*`, set per-draw from the
/// register-derived rect) and, when `vertex_layout` is `Some`, a single interleaved
/// vertex-buffer binding (the register-derived V# fetch); `None` is the embedded
/// `gl_VertexIndex` path with no vertex input. No descriptors, a single RGBA color
/// attachment. All core Vulkan 1.0 + `VK_DYNAMIC_STATE_VIEWPORT/SCISSOR` (Vulkan 1.0
/// core), portable subset (decision-3).
///
/// # Safety
/// `ctx.device` must be live and owned by the caller; `vs_spv`/`ps_spv` are valid
/// SPIR-V modules.
#[allow(clippy::too_many_arguments)]
unsafe fn create_host_pipeline(
    ctx: &VulkanContext,
    render_pass: vk::RenderPass,
    vs_spv: &[u32],
    ps_spv: &[u32],
    vertex_layout: Option<ps4_core::gpu::VertexLayout>,
    vertex_storage: &[StorageBinding],
    push_constants: Option<PushConstantRange>,
    textures: &[TextureBinding],
    const_storage: Option<StorageBinding>,
    const_storage_fragment: Option<StorageBinding>,
    blend: BlendKey,
    topology: CorePrimitiveTopology,
) -> HostPipeline {
    unsafe {
        let vert_module = create_shader_module(&ctx.device, vs_spv);
        let frag_module = create_shader_module(&ctx.device, ps_spv);
        let main_name = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo {
                s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
                stage: vk::ShaderStageFlags::VERTEX,
                module: vert_module,
                p_name: main_name.as_ptr(),
                ..Default::default()
            },
            vk::PipelineShaderStageCreateInfo {
                s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
                stage: vk::ShaderStageFlags::FRAGMENT,
                module: frag_module,
                p_name: main_name.as_ptr(),
                ..Default::default()
            },
        ];

        // Vertex input: declare one binding per referenced vertex buffer (its stride) and
        // one attribute per derived V# (location/binding/format/offset), from the
        // register-derived layout — not a hardcoded vec4. `None` = the embedded VS reads
        // gl_VertexIndex: empty vertex input (unchanged). The `dfmt`/`nfmt` → vk::Format
        // half of the mapping is `vk_vertex_format`; the layout carried the Vulkan-free
        // half. A storage-fetch VS consumes NO vertex-input (it fetches from the SSBO by
        // gl_VertexIndex), so the vertex-input state is empty regardless of `vertex_layout`
        // — a phantom attribute the VS never reads would be an invalid pipeline. Multi-stream
        // (task-153): any vertex-pull binding present means the VS is SSBO-fetch, so empty.
        let vtx_bindings: Vec<vk::VertexInputBindingDescription> = if !vertex_storage.is_empty() {
            Vec::new()
        } else {
            vertex_layout
                .iter()
                .flat_map(|vl| vl.bindings())
                .map(|b| vk::VertexInputBindingDescription {
                    binding: b.binding,
                    stride: b.stride,
                    input_rate: vk::VertexInputRate::VERTEX,
                })
                .collect()
        };
        let vtx_attrs: Vec<vk::VertexInputAttributeDescription> = if !vertex_storage.is_empty() {
            Vec::new()
        } else {
            vertex_layout
                .iter()
                .flat_map(|vl| vl.attributes())
                .map(|a| vk::VertexInputAttributeDescription {
                    location: a.location,
                    binding: a.binding,
                    format: vk_vertex_format(a.format),
                    offset: a.offset,
                })
                .collect()
        };
        let vertex_input = vk::PipelineVertexInputStateCreateInfo {
            s_type: vk::StructureType::PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
            vertex_binding_description_count: vtx_bindings.len() as u32,
            p_vertex_binding_descriptions: vtx_bindings.as_ptr(),
            vertex_attribute_description_count: vtx_attrs.len() as u32,
            p_vertex_attribute_descriptions: vtx_attrs.as_ptr(),
            ..Default::default()
        };
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo {
            s_type: vk::StructureType::PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
            // From `VGT_PRIMITIVE_TYPE` (task-184). A GCN rect list is issued as a
            // 4-vertex triangle strip covering the same parallelogram — see
            // `ps4_core::gpu::PrimitiveTopology`.
            topology: match topology {
                CorePrimitiveTopology::TriangleList => vk::PrimitiveTopology::TRIANGLE_LIST,
                CorePrimitiveTopology::TriangleStrip => vk::PrimitiveTopology::TRIANGLE_STRIP,
            },
            ..Default::default()
        };
        // Viewport/scissor are DYNAMIC: counts are 1 but the actual rects are set per-draw
        // via cmd_set_viewport/scissor from the register-derived data.
        let viewport_state = vk::PipelineViewportStateCreateInfo {
            s_type: vk::StructureType::PIPELINE_VIEWPORT_STATE_CREATE_INFO,
            viewport_count: 1,
            scissor_count: 1,
            ..Default::default()
        };
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state = vk::PipelineDynamicStateCreateInfo {
            s_type: vk::StructureType::PIPELINE_DYNAMIC_STATE_CREATE_INFO,
            dynamic_state_count: dynamic_states.len() as u32,
            p_dynamic_states: dynamic_states.as_ptr(),
            ..Default::default()
        };
        let rasterizer = vk::PipelineRasterizationStateCreateInfo {
            s_type: vk::StructureType::PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
            line_width: 1.0,
            cull_mode: vk::CullModeFlags::NONE,
            front_face: vk::FrontFace::COUNTER_CLOCKWISE,
            polygon_mode: vk::PolygonMode::FILL,
            ..Default::default()
        };
        let multisampling = vk::PipelineMultisampleStateCreateInfo {
            s_type: vk::StructureType::PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
            rasterization_samples: vk::SampleCountFlags::TYPE_1,
            ..Default::default()
        };
        let color_blend_attachment = [blend_attachment_state(blend)];
        let color_blending = vk::PipelineColorBlendStateCreateInfo {
            s_type: vk::StructureType::PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
            attachment_count: 1,
            p_attachments: color_blend_attachment.as_ptr(),
            ..Default::default()
        };

        // Pipeline layout. The embedded path binds no resources → an empty layout
        // (unchanged). A recompiled VS that fetches through SSBOs needs one STORAGE_BUFFER
        // binding PER V# stream (VERTEX stage) plus a push-constant range (VERTEX stage) for
        // the per-stream num_records/stride/dst_sel groups; a pixel shader that samples a
        // texture needs a COMBINED_IMAGE_SAMPLER binding (FRAGMENT stage). All live in the one
        // set-0 layout — a pipeline whose SPIR-V declares any must declare it here or the
        // driver faults.
        let mut dsl_bindings: Vec<vk::DescriptorSetLayoutBinding> = Vec::new();
        // Multi-stream vertex fetch (task-153): one STORAGE_BUFFER binding per V# stream, each
        // at its own binding index (e.g. 0/3/4/5), all VERTEX stage.
        for s in vertex_storage {
            dsl_bindings.push(vk::DescriptorSetLayoutBinding {
                binding: s.binding,
                descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::VERTEX,
                ..Default::default()
            });
        }
        // The scalar constant buffer(s) (s_buffer_load): a STORAGE_BUFFER binding per stage
        // that reads constants, distinct from the vertex-pull `storage` above. All live in
        // set-0. The stage flags MUST match the shader whose SPIR-V declares each SSBO or
        // vkCreateGraphicsPipelines faults (VUID-VkGraphicsPipelineCreateInfo-layout-07988) →
        // a garbage pipeline handle that segfaults the driver at draw/present (task-139). Two
        // distinct slots (task-174): the VS const at set0/bind2 (VERTEX) and the PS const at
        // set0/bind6 (FRAGMENT) — a draw whose VS AND PS both load constants declares both,
        // with no binding collision.
        if let Some(cb) = const_storage {
            dsl_bindings.push(vk::DescriptorSetLayoutBinding {
                binding: cb.binding,
                descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::VERTEX,
                ..Default::default()
            });
        }
        if let Some(cb) = const_storage_fragment {
            dsl_bindings.push(vk::DescriptorSetLayoutBinding {
                binding: cb.binding,
                descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::FRAGMENT,
                ..Default::default()
            });
        }
        // One COMBINED_IMAGE_SAMPLER per texture the PS samples (task-199). A shader that
        // mixes a register-resident T# with a memory-resident one declares two, at two
        // distinct bindings; the layout must list every one the SPIR-V declares or the
        // driver faults exactly as it does for a missing SSBO.
        for t in textures {
            dsl_bindings.push(vk::DescriptorSetLayoutBinding {
                binding: t.binding,
                descriptor_type: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::FRAGMENT,
                ..Default::default()
            });
        }
        let set_layout = (!dsl_bindings.is_empty()).then(|| {
            let dsl_info = vk::DescriptorSetLayoutCreateInfo {
                s_type: vk::StructureType::DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                binding_count: dsl_bindings.len() as u32,
                p_bindings: dsl_bindings.as_ptr(),
                ..Default::default()
            };
            ctx.device
                .create_descriptor_set_layout(&dsl_info, None)
                .unwrap()
        });
        let set_layouts: Vec<vk::DescriptorSetLayout> = set_layout.into_iter().collect();
        let pc_ranges: Vec<vk::PushConstantRange> = push_constants
            .into_iter()
            .map(|pc| vk::PushConstantRange {
                stage_flags: vk::ShaderStageFlags::VERTEX,
                offset: pc.offset,
                size: pc.size,
            })
            .collect();
        let layout_info = vk::PipelineLayoutCreateInfo {
            s_type: vk::StructureType::PIPELINE_LAYOUT_CREATE_INFO,
            set_layout_count: set_layouts.len() as u32,
            p_set_layouts: set_layouts.as_ptr(),
            push_constant_range_count: pc_ranges.len() as u32,
            p_push_constant_ranges: pc_ranges.as_ptr(),
            ..Default::default()
        };
        let layout = ctx
            .device
            .create_pipeline_layout(&layout_info, None)
            .unwrap();

        let pipeline_info = vk::GraphicsPipelineCreateInfo {
            s_type: vk::StructureType::GRAPHICS_PIPELINE_CREATE_INFO,
            stage_count: 2,
            p_stages: stages.as_ptr(),
            p_vertex_input_state: &vertex_input,
            p_input_assembly_state: &input_assembly,
            p_viewport_state: &viewport_state,
            p_rasterization_state: &rasterizer,
            p_multisample_state: &multisampling,
            p_color_blend_state: &color_blending,
            p_dynamic_state: &dynamic_state,
            layout,
            render_pass,
            subpass: 0,
            ..Default::default()
        };
        let pipeline = ctx
            .device
            .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
            .unwrap()[0];

        ctx.device.destroy_shader_module(vert_module, None);
        ctx.device.destroy_shader_module(frag_module, None);

        HostPipeline {
            pipeline,
            layout,
            set_layout,
            // Overwritten by `create_pipeline` from the binding options.
            needs_const: 0,
            needs_storage: 0,
            needs_texture: 0,
        }
    }
}

/// Map a GFX6/Sea-Islands `CB_BLEND0_CONTROL` `BLEND_*` factor enum (the value packed
/// into a `*_SRCBLEND`/`*_DESTBLEND` field) to the matching [`vk::BlendFactor`].
///
/// The enum layout is the public AMD Sea-Islands (GFX6) blend-factor table. The two
/// live Celeste controls anchor it: `0x45010501` decodes COLOR_SRCBLEND=1 /
/// COLOR_DESTBLEND=5 (premultiplied `ONE`, `ONE_MINUS_SRC_ALPHA`) and `0x41040104`
/// decodes COLOR_SRCBLEND=4 / COLOR_DESTBLEND=1 (additive `SRC_ALPHA`, `ONE`) — both
/// consistent with this table. Only the range the corpus exercises is mapped precisely;
/// higher values (constant-color/factor, saturate) fall through to `ONE` until a title
/// programs them, which keeps the match total and non-panicking (a defensive floor).
fn vk_blend_factor(factor: u32) -> vk::BlendFactor {
    match factor {
        0 => vk::BlendFactor::ZERO,
        1 => vk::BlendFactor::ONE,
        2 => vk::BlendFactor::SRC_COLOR,
        3 => vk::BlendFactor::ONE_MINUS_SRC_COLOR,
        4 => vk::BlendFactor::SRC_ALPHA,
        5 => vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        6 => vk::BlendFactor::DST_ALPHA,
        7 => vk::BlendFactor::ONE_MINUS_DST_ALPHA,
        8 => vk::BlendFactor::DST_COLOR,
        9 => vk::BlendFactor::ONE_MINUS_DST_COLOR,
        10 => vk::BlendFactor::SRC_ALPHA_SATURATE,
        _ => vk::BlendFactor::ONE,
    }
}

/// Map a GFX6/Sea-Islands `CB_BLEND0_CONTROL` `*_COMB_FCN` equation enum to the matching
/// [`vk::BlendOp`]. Both live Celeste controls program COMB_FCN=0 (`ADD`); the rest of
/// the table is the public GFX6 combine-function order. Unknown values fall through to
/// `ADD` so the match stays total.
fn vk_blend_op(comb: u32) -> vk::BlendOp {
    match comb {
        0 => vk::BlendOp::ADD,
        1 => vk::BlendOp::SUBTRACT,
        2 => vk::BlendOp::MIN,
        3 => vk::BlendOp::MAX,
        4 => vk::BlendOp::REVERSE_SUBTRACT,
        _ => vk::BlendOp::ADD,
    }
}

/// Translate a Vulkan-free [`BlendKey`] (MRT0 blend-enable bit + raw `CB_BLEND0_CONTROL`
/// word) into the `vk::PipelineColorBlendAttachmentState` a pipeline declares (task-154).
///
/// Prior to this the pipeline hardcoded `blend_enable: 0`, so every draw fully overwrote
/// its pixels; Celeste's premultiplied-alpha layer compositing collapsed and the scene
/// rendered with RGB≈0. The `BlendKey` carries the register verbatim, so the GFX6 field
/// decode happens here:
/// - `blend_enable` = `BlendKey::enable` (`CB_BLEND0_CONTROL.ENABLE`, bit 30 — matches
///   `derive_blend`).
/// - COLOR: COLOR_SRCBLEND `[4:0]`, COLOR_COMB_FCN `[7:5]`, COLOR_DESTBLEND `[12:8]`.
/// - ALPHA: when SEPARATE_ALPHA_BLEND (bit 29) is set, ALPHA_SRCBLEND `[18:16]`,
///   ALPHA_COMB_FCN `[21:19]`, ALPHA_DESTBLEND `[26:24]`; otherwise alpha mirrors color
///   (both live Celeste controls have the bit clear, so alpha mirrors color there).
///
/// When blending is disabled the attachment matches the old hardcoded state exactly
/// (`blend_enable: 0`, RGBA write mask), so opaque pipelines — e.g. the textured-quad
/// example whose control has the enable bit clear — build identically.
fn blend_attachment_state(blend: BlendKey) -> vk::PipelineColorBlendAttachmentState {
    // The guest's own MRT0 write mask (`CB_TARGET_MASK`), not a hardcoded RGBA: masking
    // alpha off is how a premultiplied-alpha intermediate keeps the alpha its clear left
    // there (see `BlendKey::write_mask`).
    let mut write_mask = vk::ColorComponentFlags::empty();
    for (bit, flag) in [
        (0, vk::ColorComponentFlags::R),
        (1, vk::ColorComponentFlags::G),
        (2, vk::ColorComponentFlags::B),
        (3, vk::ColorComponentFlags::A),
    ] {
        if blend.write_mask & (1 << bit) != 0 {
            write_mask |= flag;
        }
    }
    if !blend.enable {
        return vk::PipelineColorBlendAttachmentState {
            color_write_mask: write_mask,
            blend_enable: 0,
            ..Default::default()
        };
    }
    // The bit layout (and the SEPARATE_ALPHA_BLEND mirroring rule) lives once, on
    // `BlendKey`, so the GPU-state snapshot's decoded blend names the same fields this
    // pipeline is built from rather than a private second decode of the same word.
    let f = blend.fields();
    vk::PipelineColorBlendAttachmentState {
        color_write_mask: write_mask,
        blend_enable: 1,
        src_color_blend_factor: vk_blend_factor(f.color_src),
        dst_color_blend_factor: vk_blend_factor(f.color_dst),
        color_blend_op: vk_blend_op(f.color_comb),
        src_alpha_blend_factor: vk_blend_factor(f.alpha_src),
        dst_alpha_blend_factor: vk_blend_factor(f.alpha_dst),
        alpha_blend_op: vk_blend_op(f.alpha_comb),
    }
}

/// Map a Vulkan-free [`VertexFormat`] to the concrete `vk::Format` a vertex-attribute
/// description declares (doc-2 §C4). This is the `vk::*` half of the `dfmt`/`nfmt` →
/// format mapping; the [`VertexFormat`] identity was derived Vulkan-free in `ps4-gnm`.
///
/// [`VertexFormat::Unsupported`] never reaches here: the `ps4-gnm` derivation defers a
/// draw whose `dfmt`/`nfmt` maps to no host format before building the pipeline key, so
/// no `CreatePipeline` is emitted for it. It maps to `UNDEFINED` here purely so the match
/// is total and non-panicking — a defensive floor, not a live path.
fn vk_vertex_format(format: ps4_core::gpu::VertexFormat) -> vk::Format {
    use ps4_core::gpu::VertexFormat;
    match format {
        VertexFormat::R32Sfloat => vk::Format::R32_SFLOAT,
        VertexFormat::R32G32Sfloat => vk::Format::R32G32_SFLOAT,
        VertexFormat::R32G32B32Sfloat => vk::Format::R32G32B32_SFLOAT,
        VertexFormat::R32G32B32A32Sfloat => vk::Format::R32G32B32A32_SFLOAT,
        VertexFormat::R32Uint => vk::Format::R32_UINT,
        VertexFormat::R32G32B32A32Uint => vk::Format::R32G32B32A32_UINT,
        VertexFormat::R32Sint => vk::Format::R32_SINT,
        VertexFormat::R32G32B32A32Sint => vk::Format::R32G32B32A32_SINT,
        VertexFormat::R8G8B8A8Unorm => vk::Format::R8G8B8A8_UNORM,
        VertexFormat::R16G16Unorm => vk::Format::R16G16_UNORM,
        VertexFormat::Unsupported => vk::Format::UNDEFINED,
    }
}

/// Map a Vulkan-free [`TextureFormat`] to the `vk::Format` a sampled image is created in
/// (doc-2 §C3). Both are 8-bit RGBA; the `_SRGB` variant makes `OpImageSample` auto-decode
/// the texel to LINEAR so the fragment shader composites in linear space (task-154 residual
/// #2). Both are core Vulkan / MoltenVK-portable (task-133).
fn vk_texture_format(format: TextureFormat) -> vk::Format {
    match format {
        TextureFormat::R8G8B8A8Unorm => vk::Format::R8G8B8A8_UNORM,
        TextureFormat::R8G8B8A8Srgb => vk::Format::R8G8B8A8_SRGB,
    }
}

/// The guest-memory [`SurfaceLayout`] an RT readback must pack into (task-181), or `None`
/// when the guest surface is one this repo cannot express — in which case the readback
/// REFUSES rather than writing bytes no reader can decode.
///
/// The extent is the guest ROW STRIDE (`pitch`), not the RT's content width: the host image
/// carries only the content extent (task-180), so the padded columns have to be reintroduced
/// here or every row after the first lands skewed. [`pack_guest_surface`] pads the rows out;
/// this only names the layout they are packed into.
///
/// `None` is returned for 2D macro-tiling (`tile_mode_index >= 9`, which has no re-tiler —
/// see [`ps4_core::tiling::tile_kind`]) and for a nonsensical `pitch < width`.
fn guest_surface_layout(
    w: u32,
    h: u32,
    pitch: u32,
    tiling: ps4_core::gpu::Tiling,
) -> Option<SurfaceLayout> {
    if pitch < w || w == 0 || h == 0 {
        return None;
    }
    let index = match tiling {
        ps4_core::gpu::Tiling::Linear => 0,
        ps4_core::gpu::Tiling::Tiled { tile_mode_index } => {
            // A mode index wider than a byte is not a tile mode at all; classify it as the
            // deferred macro case rather than truncating it into a mode we would then "handle".
            u8::try_from(tile_mode_index).ok()?
        }
    };
    let packed = match ps4_core::tiling::tile_kind(index) {
        // Both linear kinds pack identically once the rows carry their padding: the row
        // stride IS the extent width here, so linear-general row-major is already the
        // pitch-strided layout linear-aligned describes.
        ps4_core::tiling::TileKind::Linear | ps4_core::tiling::TileKind::LinearAligned => {
            Tiling::LinearGeneral
        }
        ps4_core::tiling::TileKind::Thin1d => Tiling::Thin1d,
        ps4_core::tiling::TileKind::Macro2d => return None,
    };
    Some(SurfaceLayout {
        texel: TexelSize::Bpp32,
        extent: Extent {
            width: pitch,
            height: h,
        },
        tiling: packed,
        compression: Compression::Off,
        // The stride is already the extent width, so no separate decoded pitch applies —
        // neither re-tile branch selected above consults this field (task-155).
        pitch: 0,
    })
}

/// Pack the RT's `w`x`h` linear RGBA8 content into the guest surface `layout`
/// [`guest_surface_layout`] named: pad each row out to the layout's stride, then re-tile
/// (task-181). The inverse of the upload-path detile, so
/// `detile(pack_guest_surface(content)) == content` over the content columns.
///
/// The padding columns (and, for 1D-thin, the texels of any micro-tile row past `h`) are
/// written as ZERO. They are surface padding no reader samples, but the write does clobber
/// whatever the guest left there — acceptable for an opt-in diagnostic, and stated so the
/// next reader does not mistake a zeroed pad for rendered black.
fn pack_guest_surface(
    content: &[u8],
    w: u32,
    h: u32,
    layout: &SurfaceLayout,
) -> Result<Vec<u8>, ps4_gnm::cache::tile::TileError> {
    const BPP: usize = 4;
    let stride = layout.extent.width as usize;
    let (w, h) = (w as usize, h as usize);
    let padded = if stride == w {
        content.to_vec()
    } else {
        let mut padded = vec![0u8; stride * h * BPP];
        for y in 0..h {
            let src = y * w * BPP;
            let dst = y * stride * BPP;
            let row = content.get(src..src + w * BPP).ok_or(
                ps4_gnm::cache::tile::TileError::ShortBuffer {
                    got: content.len(),
                    expected: w * h * BPP,
                },
            )?;
            padded[dst..dst + w * BPP].copy_from_slice(row);
        }
        padded
    };
    ps4_gnm::cache::tile::tile(&padded, layout)
}

/// Map a Vulkan-free [`ColorFormat`] to the `vk::Format` an offscreen render target is
/// created in (doc-2 §8.5, task-56). The RT-as-texture image is one-image-both-roles
/// (COLOR_ATTACHMENT | SAMPLED), created here per the color format the executor derived from
/// the guest's `CB_COLOR0_*` regs. [`ColorFormat::Unsupported`] never reaches here — the
/// executor defers a draw whose RT format maps to no host format before emitting
/// `CreateRenderTarget` — so it maps to `UNDEFINED` purely to keep the match total (a
/// defensive floor, not a live path).
fn vk_color_format(format: ColorFormat) -> vk::Format {
    match format {
        ColorFormat::B8G8R8A8Unorm => vk::Format::B8G8R8A8_UNORM,
        ColorFormat::R8G8B8A8Unorm => vk::Format::R8G8B8A8_UNORM,
        ColorFormat::Unsupported => vk::Format::UNDEFINED,
    }
}

/// Map a Vulkan-free [`SamplerFilter`] to `vk::Filter` (doc-2 §C4).
fn vk_filter(filter: ps4_core::gpu::SamplerFilter) -> vk::Filter {
    use ps4_core::gpu::SamplerFilter;
    match filter {
        SamplerFilter::Nearest => vk::Filter::NEAREST,
        SamplerFilter::Linear => vk::Filter::LINEAR,
    }
}

/// Map a Vulkan-free [`SamplerAddressMode`] to `vk::SamplerAddressMode` (doc-2 §C4).
fn vk_address_mode(mode: ps4_core::gpu::SamplerAddressMode) -> vk::SamplerAddressMode {
    use ps4_core::gpu::SamplerAddressMode;
    match mode {
        SamplerAddressMode::Repeat => vk::SamplerAddressMode::REPEAT,
        SamplerAddressMode::MirrorRepeat => vk::SamplerAddressMode::MIRRORED_REPEAT,
        SamplerAddressMode::ClampToEdge => vk::SamplerAddressMode::CLAMP_TO_EDGE,
    }
}

/// Build a `VkShaderModule` from SPIR-V words. The words arrive as the
/// `Arc<[u32]>` payload a `CreatePipeline` command carried (host-endian, already
/// validated upstream), so no byte-reinterpret is needed.
///
/// # Safety
/// `device` must be live; `code` must be a valid SPIR-V module.
unsafe fn create_shader_module(device: &ash::Device, code: &[u32]) -> vk::ShaderModule {
    unsafe {
        let info = vk::ShaderModuleCreateInfo {
            s_type: vk::StructureType::SHADER_MODULE_CREATE_INFO,
            code_size: std::mem::size_of_val(code),
            p_code: code.as_ptr(),
            ..Default::default()
        };
        device.create_shader_module(&info, None).unwrap()
    }
}

/// Record ONE resolved pass into the shared command buffer `cb` (doc-2 §8.5, task-56 step
/// 4). Begins `render_pass`/`framebuffer` (sized to `extent`), binds the pipeline +
/// whichever descriptors the pass declared, sets the dynamic viewport/scissor (defaulting
/// to the full `extent`), and records the draw. Does NOT allocate the command buffer,
/// submit, or wait — [`AshBackend::record_passes`] records every pass into one `cb` and
/// submits once behind one fence. Returns the per-pass descriptor pool (if a set was
/// allocated) for the caller to destroy after the fence wait.
///
/// The loadOp (CLEAR vs LOAD) and final layout are baked into `render_pass` by its caller
/// (the videoout target and `create_rt_target` build their own); this records the draw into
/// whatever pass it is given. All draws leave their attachment in the render pass's declared
/// final layout.
///
/// # Safety
/// `ctx`/`cb` must be live and owned by the display thread; `pass`'s pipeline + buffers are
/// valid; `render_pass`/`framebuffer` are compatible with the pass's pipeline.
unsafe fn record_pass_into(
    ctx: &VulkanContext,
    cb: vk::CommandBuffer,
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,
    extent: vk::Extent2D,
    pass: &RecordedPass,
    prof: bool,
    // Returns the pass's descriptor pool (destroyed by the caller after the fence wait)
    // and the nanoseconds spent creating it, which the caller folds into the submit's
    // transient-object creation total. Zero when the profiler is off.
) -> (Option<vk::DescriptorPool>, u64) {
    let pipeline = pass.pipeline;
    let pipeline_layout = pass.pipeline_layout;
    let set_layout = pass.set_layout;
    let draw = &pass.draw;
    let vertex_buffers = &pass.vertex_buffers;
    let storage_binds = pass.storage_binds.as_slice();
    let const_binds = pass.const_binds.as_slice();
    let texture_binds = pass.texture_binds.as_slice();
    let viewport = pass.viewport;
    let scissor = pass.scissor;
    let mut create_ns = 0u64;
    unsafe {
        // task-179 knob (`UNEMUPS4_X_RT_CLEAR_ALPHA0=1`, default off): clear to fully
        // TRANSPARENT black instead of opaque. Our opaque clear is the floor an offscreen RT's
        // alpha can never fall below, so a target the guest wants left transparent (its bloom
        // chain clears those to (0,0,0,0)) starts at alpha 1 for us — and a premultiplied
        // composite of it then replaces the frame instead of adding to it.
        static CLEAR_A0: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let a = if *CLEAR_A0
            .get_or_init(|| std::env::var("UNEMUPS4_X_RT_CLEAR_ALPHA0").is_ok_and(|v| v != "0"))
        {
            0.0
        } else {
            1.0
        };
        let clear = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, a],
            },
        }];
        let rp_begin = vk::RenderPassBeginInfo {
            s_type: vk::StructureType::RENDER_PASS_BEGIN_INFO,
            render_pass,
            framebuffer,
            render_area: vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            },
            clear_value_count: 1,
            p_clear_values: clear.as_ptr(),
            ..Default::default()
        };
        ctx.device
            .cmd_begin_render_pass(cb, &rp_begin, vk::SubpassContents::INLINE);
        ctx.device
            .cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);

        // Descriptor-set draw: allocate ONE set-0 descriptor set (from a pool created for
        // this list, destroyed after the fence wait) and write whichever of the bindings the
        // pipeline declared into it — a STORAGE_BUFFER for a vertex-pull VS and/or a
        // COMBINED_IMAGE_SAMPLER for a texture-sampling PS. Mirrors diff_harness's render_vs.
        // The pool is `Some` only when the pipeline has a set layout AND at least one binding
        // to write; otherwise the draw records without descriptors (embedded path).
        let has_binding =
            !storage_binds.is_empty() || !const_binds.is_empty() || !texture_binds.is_empty();
        let desc_pool: Option<vk::DescriptorPool> = match (has_binding, set_layout) {
            (true, Some(dsl)) => {
                let mut sizes: Vec<vk::DescriptorPoolSize> = Vec::new();
                // Every vertex-pull stream (task-153) and each constant buffer (task-174) are
                // STORAGE_BUFFERs.
                let storage_count = storage_binds.len() as u32 + const_binds.len() as u32;
                if storage_count > 0 {
                    sizes.push(vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::STORAGE_BUFFER,
                        descriptor_count: storage_count,
                    });
                }
                if !texture_binds.is_empty() {
                    sizes.push(vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        descriptor_count: texture_binds.len() as u32,
                    });
                }
                let pool_info = vk::DescriptorPoolCreateInfo {
                    s_type: vk::StructureType::DESCRIPTOR_POOL_CREATE_INFO,
                    max_sets: 1,
                    pool_size_count: sizes.len() as u32,
                    p_pool_sizes: sizes.as_ptr(),
                    ..Default::default()
                };
                let tc = prof.then(Instant::now);
                let pool = ctx.device.create_descriptor_pool(&pool_info, None).unwrap();
                if let Some(tc) = tc {
                    create_ns = tc.elapsed().as_nanos() as u64;
                }
                let layouts = [dsl];
                let alloc_info = vk::DescriptorSetAllocateInfo {
                    s_type: vk::StructureType::DESCRIPTOR_SET_ALLOCATE_INFO,
                    descriptor_pool: pool,
                    descriptor_set_count: 1,
                    p_set_layouts: layouts.as_ptr(),
                    ..Default::default()
                };
                let dset = ctx.device.allocate_descriptor_sets(&alloc_info).unwrap()[0];

                // The write structs reference these infos by raw pointer, so every one must
                // outlive `update_descriptor_sets`. Multi-stream (task-153): one
                // `[DescriptorBufferInfo; 1]` per V# stream, all held in this Vec so none is
                // dropped before the update reads the pointers.
                let storage_buf_infos: Vec<[vk::DescriptorBufferInfo; 1]> = storage_binds
                    .iter()
                    .map(|sb| {
                        [vk::DescriptorBufferInfo {
                            buffer: sb.buffer,
                            offset: 0,
                            range: vk::WHOLE_SIZE,
                        }]
                    })
                    .collect();
                // One buffer-info per const bind (task-174): the VS const and/or the PS const.
                // A Vec (like `storage_buf_infos`) so each `p_buffer_info` points at storage
                // that outlives the `writes` push below.
                let const_buf_infos: Vec<[vk::DescriptorBufferInfo; 1]> = const_binds
                    .iter()
                    .map(|cb| {
                        [vk::DescriptorBufferInfo {
                            buffer: cb.buffer,
                            offset: 0,
                            range: vk::WHOLE_SIZE,
                        }]
                    })
                    .collect();
                // One image-info per bound texture; kept alive for the whole
                // `update_descriptor_sets` call because the writes point into it.
                let img_infos: Vec<[vk::DescriptorImageInfo; 1]> = texture_binds
                    .iter()
                    .map(|tb| {
                        [vk::DescriptorImageInfo {
                            sampler: tb.sampler,
                            image_view: tb.view,
                            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        }]
                    })
                    .collect();
                let mut writes: Vec<vk::WriteDescriptorSet> = Vec::new();
                // One STORAGE_BUFFER write per V# stream, each at its own binding (task-153).
                for (sb, bi) in storage_binds.iter().zip(storage_buf_infos.iter()) {
                    writes.push(vk::WriteDescriptorSet {
                        s_type: vk::StructureType::WRITE_DESCRIPTOR_SET,
                        dst_set: dset,
                        dst_binding: sb.binding,
                        descriptor_count: 1,
                        descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
                        p_buffer_info: bi.as_ptr(),
                        ..Default::default()
                    });
                }
                // One STORAGE_BUFFER write per const bind, each at its own binding (task-174):
                // the VS const at set0/bind2, the PS const at set0/bind6.
                for (cb, bi) in const_binds.iter().zip(const_buf_infos.iter()) {
                    writes.push(vk::WriteDescriptorSet {
                        s_type: vk::StructureType::WRITE_DESCRIPTOR_SET,
                        dst_set: dset,
                        dst_binding: cb.binding,
                        descriptor_count: 1,
                        descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
                        p_buffer_info: bi.as_ptr(),
                        ..Default::default()
                    });
                }
                // One COMBINED_IMAGE_SAMPLER write per texture, each at its own binding
                // (task-199) — the analogue of the per-stream / per-const loops above.
                for (tb, ii) in texture_binds.iter().zip(img_infos.iter()) {
                    writes.push(vk::WriteDescriptorSet {
                        s_type: vk::StructureType::WRITE_DESCRIPTOR_SET,
                        dst_set: dset,
                        dst_binding: tb.binding,
                        descriptor_count: 1,
                        descriptor_type: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        p_image_info: ii.as_ptr(),
                        ..Default::default()
                    });
                }
                ctx.device.update_descriptor_sets(&writes, &[]);
                let sets = [dset];
                ctx.device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipeline_layout,
                    0,
                    &sets,
                    &[],
                );
                // A vertex-pull VS reads each stream's { uint num_records; uint stride; uint
                // dst_sel; uint format; } group from the push-constant block; a texture-only
                // draw declares none, so push only for present streams. Multi-stream (task-153):
                // each stream writes its 16-byte group at its own `pc_offset` (= 16*stream) —
                // member 0 = num_records (fetch clamp), member 1 = stride (vertex element stride,
                // task-140), member 2 = dst_sel (destination swizzle, task-155), member 3 =
                // format (packed dfmt/nfmt the fetch unpacks each component with, task-164).
                for sb in storage_binds {
                    let mut pc = [0u8; 16];
                    pc[0..4].copy_from_slice(&sb.num_records.to_le_bytes());
                    pc[4..8].copy_from_slice(&sb.stride.to_le_bytes());
                    pc[8..12].copy_from_slice(&sb.dst_sel.to_le_bytes());
                    pc[12..16].copy_from_slice(&sb.format.to_le_bytes());
                    ctx.device.cmd_push_constants(
                        cb,
                        pipeline_layout,
                        vk::ShaderStageFlags::VERTEX,
                        sb.pc_offset,
                        &pc,
                    );
                }
                Some(pool)
            }
            _ => None,
        };

        // Dynamic viewport/scissor: use the register-derived rect, defaulting to the full
        // target extent when the draw programmed none. A negative viewport height is the
        // portable Vulkan Y-flip (decision-3) — passed straight through. A zero-area rect is
        // treated as "unprogrammed": the embedded fullscreen corpus sets no PA_CL_VPORT /
        // scissor registers, so its derived rect is all-zeros; drawing into a 0x0 viewport
        // rasterizes nothing, so it too falls back to the full target. The fallback uses the
        // pass's own `extent` (RES for videoout, the RT size for an offscreen pass), so an
        // unprogrammed offscreen draw fills its RT rather than a mis-sized videoout rect.
        let full_vp = ViewportRect {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
        };
        let vp = match viewport {
            Some(v) if v.width != 0.0 && v.height != 0.0 => v,
            _ => full_vp,
        };
        let viewports = [vk::Viewport {
            x: vp.x,
            y: vp.y,
            width: vp.width,
            height: vp.height,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let full_sc = ScissorRect {
            x: 0,
            y: 0,
            width: extent.width,
            height: extent.height,
        };
        let sc = match scissor {
            Some(s) if s.width != 0 && s.height != 0 => s,
            _ => full_sc,
        };
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: sc.x, y: sc.y },
            extent: vk::Extent2D {
                width: sc.width,
                height: sc.height,
            },
        }];
        ctx.device.cmd_set_viewport(cb, 0, &viewports);
        ctx.device.cmd_set_scissor(cb, 0, &scissors);

        // Bind each register-derived vertex buffer at its slot (offset 0 — the V# base is
        // the buffer base). No vertex buffers = the embedded gl_VertexIndex path.
        for vb in vertex_buffers {
            let bufs = [vb.buffer];
            let offsets = [0u64];
            ctx.device
                .cmd_bind_vertex_buffers(cb, vb.slot, &bufs, &offsets);
        }

        match *draw {
            DrawCall::Auto { vertex_count } => {
                ctx.device.cmd_draw(cb, vertex_count, 1, 0, 0);
            }
            DrawCall::Indexed {
                buffer,
                index_count,
                index_type,
            } => {
                let vk_index_type = match index_type {
                    IndexType::U16 => vk::IndexType::UINT16,
                    IndexType::U32 => vk::IndexType::UINT32,
                };
                ctx.device
                    .cmd_bind_index_buffer(cb, buffer, 0, vk_index_type);
                ctx.device.cmd_draw_indexed(cb, index_count, 1, 0, 0, 0);
            }
        }

        ctx.device.cmd_end_render_pass(cb);
        // The command buffer is submitted + waited once for the whole multi-pass list by
        // `record_passes`; the per-pass descriptor pool is returned for it to free then.
        (desc_pool, create_ns)
    }
}

/// Build a transient render pass + framebuffer that renders into an offscreen render target
/// `view` sized `extent` (doc-2 §8.5, task-56 step 4). `initial_layout` is the RT's tracked
/// `current_layout`; `first_use` (an UNDEFINED tracked layout) selects loadOp=CLEAR — a
/// reuse selects loadOp=LOAD to preserve the RT's prior contents. The final layout is
/// COLOR_ATTACHMENT_OPTIMAL; the caller barriers it to SHADER_READ after the draw. RGBA8,
/// single-sample, all core Vulkan 1.0 (portable subset). Destroyed after the fence wait.
///
/// # Safety
/// `ctx` must be live and owned by the display thread; `view` is a live COLOR-aspect view of
/// an RT image of at least `extent`.
unsafe fn create_rt_target(
    ctx: &VulkanContext,
    view: vk::ImageView,
    extent: vk::Extent2D,
    initial_layout: vk::ImageLayout,
    first_use: bool,
) -> (vk::RenderPass, vk::Framebuffer) {
    unsafe {
        // First use: the RT holds no meaningful contents (UNDEFINED), so CLEAR from
        // UNDEFINED. A reuse: LOAD from the layout the prior submit left it in (SHADER_READ),
        // preserving cross-frame accumulation. The initial_layout MUST equal the RT's tracked
        // layout or the transition is a validation fault (task-56 SYNC race guard).
        let (load_op, initial) = if first_use {
            (vk::AttachmentLoadOp::CLEAR, vk::ImageLayout::UNDEFINED)
        } else {
            (vk::AttachmentLoadOp::LOAD, initial_layout)
        };
        let attachments = [vk::AttachmentDescription {
            format: EMBEDDED_TARGET_FORMAT,
            samples: vk::SampleCountFlags::TYPE_1,
            load_op,
            store_op: vk::AttachmentStoreOp::STORE,
            stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
            stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
            initial_layout: initial,
            final_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            flags: vk::AttachmentDescriptionFlags::empty(),
        }];
        let color_ref = [vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }];
        let subpass = [vk::SubpassDescription {
            pipeline_bind_point: vk::PipelineBindPoint::GRAPHICS,
            color_attachment_count: 1,
            p_color_attachments: color_ref.as_ptr(),
            ..Default::default()
        }];
        let rp_info = vk::RenderPassCreateInfo {
            s_type: vk::StructureType::RENDER_PASS_CREATE_INFO,
            attachment_count: 1,
            p_attachments: attachments.as_ptr(),
            subpass_count: 1,
            p_subpasses: subpass.as_ptr(),
            ..Default::default()
        };
        let render_pass = ctx.device.create_render_pass(&rp_info, None).unwrap();

        let fb_attachments = [view];
        let fb_info = vk::FramebufferCreateInfo {
            s_type: vk::StructureType::FRAMEBUFFER_CREATE_INFO,
            render_pass,
            attachment_count: 1,
            p_attachments: fb_attachments.as_ptr(),
            width: extent.width.max(1),
            height: extent.height.max(1),
            layers: 1,
            ..Default::default()
        };
        let framebuffer = ctx.device.create_framebuffer(&fb_info, None).unwrap();
        (render_pass, framebuffer)
    }
}

/// The per-draw undefined-descriptor guard (task-56 step 4): a draw whose pipeline declares
/// a constant buffer / vertex SSBO / sampled texture but whose matching bind missed the
/// resource cache would leave that descriptor un-written (undefined-descriptor UB). Returns
/// `false` to drop THAT draw (the multi-pass model drops one draw, not the whole submit).
/// A `None` pipeline (nothing bound) also fails the guard.
#[allow(clippy::too_many_arguments)]
fn draw_guards_ok(
    pipeline: Option<vk::Pipeline>,
    needs_const: u32,
    has_const: u32,
    needs_storage: u32,
    has_storage: u32,
    needs_texture: u32,
    has_texture: u32,
) -> bool {
    if pipeline.is_none() {
        return false;
    }
    // EVERY declared binding must have arrived, not merely one of them: a partially-written
    // descriptor set is undefined-descriptor UB that renders a plausible wrong picture
    // rather than faulting (task-184).
    if has_const < needs_const {
        tracing::warn!(
            needs = needs_const,
            got = has_const,
            "[GPU] dropping draw: pipeline needs constant buffers but not all of their V# \
             resources resolved (cache miss)"
        );
        return false;
    }
    if has_storage < needs_storage {
        tracing::warn!(
            needs = needs_storage,
            got = has_storage,
            "[GPU] dropping draw: pipeline needs vertex SSBOs but not all of their resources \
             resolved (cache miss)"
        );
        return false;
    }
    if has_texture < needs_texture {
        tracing::warn!(
            "[GPU] dropping draw: pipeline needs a texture but its image/sampler was not \
             resolved (cache miss)"
        );
        return false;
    }
    true
}

/// Bound on the per-list draw-fence wait in [`AshBackend::record_passes`]. A finite timeout so a
/// hung or faulted GPU submit cannot deadlock the display thread; on timeout the present
/// path proceeds rather than blocking forever.
const DRAW_FENCE_TIMEOUT_NS: u64 = 5_000_000_000;

/// Read the just-composited swapchain image `image_index` back to a host buffer and write
/// it as an RGBA PNG at `dump`'s destination (env-gated `UNEMUPS4_DUMP_PNG`).
/// Called after the present submit but BEFORE `queue_present`: the present render pass has
/// left the image in `PRESENT_SRC` and we still own it, so the readback captures exactly
/// the pixels the display shows without racing the presentation engine. This is the true
/// scanout (cornflower clear + sampled videoout quad), so both the embedded-draw path and
/// the pure-softgpu present path are captured. Best-effort: any Vulkan or IO failure is
/// logged and swallowed — the oracle must never break present.
fn dump_present_png(ctx: &VulkanContext, dump: &mut DumpPng, image_index: u32) {
    let out_path = if dump.is_dir {
        dump.path.join(format!("frame_{:04}.png", dump.frame))
    } else {
        dump.path.clone()
    };
    dump.frame = dump.frame.wrapping_add(1);

    let w = ctx.swapchain_extent.width;
    let h = ctx.swapchain_extent.height;
    let format = ctx.swapchain_format;
    let image = ctx.swapchain_images[image_index as usize];
    let size = (w as u64) * (h as u64) * 4;

    // SAFETY: display thread owns the device; `image` is a live swapchain image created
    // with TRANSFER_SRC usage, currently owned by us in PRESENT_SRC. Every handle below is
    // created and destroyed here.
    let pixels = unsafe {
        // Serialise against the present's render submit so the readback observes the
        // finished frame, not a race with the still-in-flight blit into this image. This
        // is a debug oracle — a full idle is fine and keeps it correct.
        let _ = ctx.device.device_wait_idle();
        let (buffer, mem) = match VulkanContext::create_buffer(
            &ctx.instance,
            &ctx.device,
            ctx.physical_device,
            size,
            vk::BufferUsageFlags::TRANSFER_DST,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("dump_png: create readback buffer failed: {e}");
                return;
            }
        };

        // One-shot copy: PRESENT_SRC -> TRANSFER_SRC, copy image->buffer, back.
        let alloc = vk::CommandBufferAllocateInfo {
            s_type: vk::StructureType::COMMAND_BUFFER_ALLOCATE_INFO,
            command_pool: ctx.command_pool,
            level: vk::CommandBufferLevel::PRIMARY,
            command_buffer_count: 1,
            ..Default::default()
        };
        // Best-effort oracle: any Vulkan error (e.g. DEVICE_LOST after a bad guest draw,
        // OUT_OF_HOST_MEMORY under load) is logged and swallowed rather than `.unwrap()`ing
        // and aborting the display thread. The already-created objects leak on the error path
        // (leak-on-exit convention); the alternative — freeing after a possibly-pending submit
        // — would be worse.
        let cb = match ctx.device.allocate_command_buffers(&alloc) {
            Ok(v) => v[0],
            Err(e) => {
                tracing::warn!("dump_png: allocate command buffer failed: {e}");
                return;
            }
        };
        let begin = vk::CommandBufferBeginInfo {
            s_type: vk::StructureType::COMMAND_BUFFER_BEGIN_INFO,
            flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
            ..Default::default()
        };
        if let Err(e) = ctx.device.begin_command_buffer(cb, &begin) {
            tracing::warn!("dump_png: begin command buffer failed: {e}");
            return;
        }

        let sub = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let to_src = vk::ImageMemoryBarrier {
            s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
            src_access_mask: vk::AccessFlags::MEMORY_READ,
            dst_access_mask: vk::AccessFlags::TRANSFER_READ,
            old_layout: vk::ImageLayout::PRESENT_SRC_KHR,
            new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            image,
            subresource_range: sub,
            ..Default::default()
        };
        ctx.device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_src],
        );

        let region = vk::BufferImageCopy {
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_extent: vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            },
            ..Default::default()
        };
        ctx.device.cmd_copy_image_to_buffer(
            cb,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            buffer,
            &[region],
        );

        let back = vk::ImageMemoryBarrier {
            s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
            src_access_mask: vk::AccessFlags::TRANSFER_READ,
            dst_access_mask: vk::AccessFlags::MEMORY_READ,
            old_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            new_layout: vk::ImageLayout::PRESENT_SRC_KHR,
            src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
            image,
            subresource_range: sub,
            ..Default::default()
        };
        ctx.device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[back],
        );

        if let Err(e) = ctx.device.end_command_buffer(cb) {
            tracing::warn!("dump_png: end command buffer failed: {e}");
            return;
        }
        let fence = match ctx
            .device
            .create_fence(&vk::FenceCreateInfo::default(), None)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("dump_png: create fence failed: {e}");
                return;
            }
        };
        let cbs = [cb];
        let submit = vk::SubmitInfo {
            s_type: vk::StructureType::SUBMIT_INFO,
            command_buffer_count: 1,
            p_command_buffers: cbs.as_ptr(),
            ..Default::default()
        };
        if let Err(e) = ctx.device.queue_submit(ctx.queue, &[submit], fence) {
            tracing::warn!("dump_png: queue submit failed: {e}");
            return;
        }
        if let Err(e) = ctx.device.wait_for_fences(&[fence], true, u64::MAX) {
            tracing::warn!("dump_png: wait for fence failed: {e}");
            return;
        }
        ctx.device.destroy_fence(fence, None);
        ctx.device.free_command_buffers(ctx.command_pool, &[cb]);

        let ptr = match ctx
            .device
            .map_memory(mem, 0, size, vk::MemoryMapFlags::empty())
        {
            Ok(p) => p as *const u8,
            Err(e) => {
                tracing::warn!("dump_png: map readback memory failed: {e}");
                return;
            }
        };
        let raw = std::slice::from_raw_parts(ptr, size as usize).to_vec();
        ctx.device.unmap_memory(mem);
        ctx.device.destroy_buffer(buffer, None);
        ctx.device.free_memory(mem, None);

        // Normalise the swapchain pixels to RGBA8 for the PNG. The surface may
        // negotiate 8-bit BGRA/RGBA *or* a packed 10-bit format (A2B10G10R10 on
        // HDR-capable displays), so convert per the actual format rather than
        // assuming 4×8-bit with an R<->B swap (task-103).
        swapchain_to_rgba8(&raw, format)
    };

    if let Err(e) = write_rgba_png(&out_path, w, h, &pixels) {
        tracing::warn!("dump_png: write {} failed: {e}", out_path.display());
    } else {
        tracing::info!("dump_png: wrote {}", out_path.display());
    }
}

/// Convert a swapchain-image readback (in `format`) to 8-bit RGBA for the PNG oracle.
///
/// The surface format is whatever the display negotiated, not necessarily 8-bit: on
/// HDR-capable displays it is often the packed 10-bit `A2B10G10R10_UNORM_PACK32`, which
/// the old code mis-read as 4×8-bit — the real cause of the videoout dump's swapped/
/// psychedelic colors (task-103). Handles the 8-bit BGRA/RGBA surfaces and the packed
/// 2-10-10-10 formats; unknown formats are passed through unchanged with a warning.
fn swapchain_to_rgba8(raw: &[u8], format: vk::Format) -> Vec<u8> {
    match format {
        vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB => raw.to_vec(),
        vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB => {
            let mut v = raw.to_vec();
            for px in v.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            v
        }
        vk::Format::A2B10G10R10_UNORM_PACK32 => unpack_a2_10_10_10(raw, false),
        vk::Format::A2R10G10B10_UNORM_PACK32 => unpack_a2_10_10_10(raw, true),
        other => {
            tracing::warn!("dump_png: unhandled swapchain format {other:?}; writing bytes as-is");
            raw.to_vec()
        }
    }
}

/// Unpack a `2-10-10-10` `PACK32` readback to RGBA8. In a Vulkan `PACK32` format the
/// first-named component sits in the most-significant bits, so for `A2B10G10R10` the
/// little-endian u32 holds R in bits 0-9, G in 10-19, B in 20-29, A in 30-31; `r_high`
/// selects the `A2R10G10B10` variant (R and B fields swapped). 10-bit channels are
/// scaled to 8-bit by dropping the low 2 bits.
fn unpack_a2_10_10_10(raw: &[u8], r_high: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for px in raw.chunks_exact(4) {
        let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
        let low = (v & 0x3FF) as u16; // bits 0-9
        let mid = ((v >> 10) & 0x3FF) as u16; // bits 10-19
        let high = ((v >> 20) & 0x3FF) as u16; // bits 20-29
        let a2 = ((v >> 30) & 0x3) as u16; // bits 30-31
        let (r, b) = if r_high { (high, low) } else { (low, high) };
        out.push((r >> 2) as u8);
        out.push((mid >> 2) as u8);
        out.push((b >> 2) as u8);
        out.push(((a2 * 255) / 3) as u8);
    }
    out
}

/// Write `w`x`h` RGBA8 `pixels` as a PNG at `path`. Self-contained encoder used only by
/// the env-gated `UNEMUPS4_DUMP_PNG` oracle: the workspace has no `png`
/// dependency and this is a headless debug tool, so the image data is stored in a single
/// uncompressed zlib block (PNG's `stored`/`type 0` DEFLATE) — no compression crate
/// needed. Output is a valid RGBA PNG the Read tool can render.
pub fn write_rgba_png(
    path: &std::path::Path,
    w: u32,
    h: u32,
    pixels: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn adler32(bytes: &[u8]) -> u32 {
        let (mut a, mut b): (u32, u32) = (1, 0);
        for &x in bytes {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc_input = Vec::with_capacity(4 + data.len());
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }

    // Raw scanlines: each row prefixed by a filter byte (0 = None).
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    let row_bytes = w as usize * 4;
    for y in 0..h as usize {
        raw.push(0);
        let start = y * row_bytes;
        raw.extend_from_slice(&pixels[start..start + row_bytes]);
    }

    // zlib stream wrapping stored DEFLATE blocks (no compression).
    let mut zlib = Vec::new();
    zlib.push(0x78); // CMF: deflate, 32K window
    zlib.push(0x01); // FLG
    let mut off = 0usize;
    while off < raw.len() {
        let remaining = raw.len() - off;
        let block = remaining.min(0xFFFF);
        let is_last = off + block >= raw.len();
        zlib.push(if is_last { 1 } else { 0 }); // BFINAL, BTYPE=00 (stored)
        zlib.extend_from_slice(&(block as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw[off..off + block]);
        off += block;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type: RGBA
    ihdr.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);

    let mut f = std::fs::File::create(path)?;
    f.write_all(&png)
}

#[cfg(test)]
mod tests {
    use super::{
        blend_attachment_state, draw_guards_ok, format_is_srgb, guest_surface_layout,
        pack_guest_surface, scanout_swap_rb, unpack_a2_10_10_10, vk_address_mode, vk_filter,
        vk_texture_format, vk_vertex_format,
    };
    use ash::vk;
    use ps4_core::gpu::{BlendKey, SamplerAddressMode, SamplerFilter, TextureFormat, VertexFormat};

    #[test]
    fn scanout_swap_rb_decodes_channel_order() {
        // task-154 residual #2: A8R8G8B8_SRGB (0x80000000, Celeste) is BGRA byte order → swap;
        // the A8B8G8R8_SRGB variant (the textured-quad example) is RGBA order → no swap.
        const A8R8G8B8_SRGB: u32 = 0x8000_0000;
        const A8B8G8R8_SRGB: u32 = 0x8000_2200;
        assert!(scanout_swap_rb(A8R8G8B8_SRGB));
        assert!(!scanout_swap_rb(A8B8G8R8_SRGB));
    }

    #[test]
    fn format_is_srgb_gates_present_decode() {
        // The present encodes linear→sRGB itself only when the swapchain is NOT _SRGB.
        assert!(format_is_srgb(vk::Format::R8G8B8A8_SRGB));
        assert!(format_is_srgb(vk::Format::B8G8R8A8_SRGB));
        assert!(!format_is_srgb(vk::Format::R8G8B8A8_UNORM));
        assert!(!format_is_srgb(vk::Format::A2R10G10B10_UNORM_PACK32));
    }

    #[test]
    fn vertex_format_maps_to_expected_vk_format() {
        // AC #1: the VertexFormat → vk::Format mapping, checked against hand-written
        // vk::Format literals (not values pulled from the mapping under test).
        assert_eq!(
            vk_vertex_format(VertexFormat::R32Sfloat),
            vk::Format::R32_SFLOAT
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R32G32Sfloat),
            vk::Format::R32G32_SFLOAT
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R32G32B32Sfloat),
            vk::Format::R32G32B32_SFLOAT
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R32G32B32A32Sfloat),
            vk::Format::R32G32B32A32_SFLOAT
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R32Uint),
            vk::Format::R32_UINT
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R32G32B32A32Uint),
            vk::Format::R32G32B32A32_UINT
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R32Sint),
            vk::Format::R32_SINT
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R32G32B32A32Sint),
            vk::Format::R32G32B32A32_SINT
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R8G8B8A8Unorm),
            vk::Format::R8G8B8A8_UNORM
        );
        assert_eq!(
            vk_vertex_format(VertexFormat::R16G16Unorm),
            vk::Format::R16G16_UNORM
        );
        // Unsupported never reaches the backend (gnm defers first); mapped to UNDEFINED
        // so the match is total and non-panicking.
        assert_eq!(
            vk_vertex_format(VertexFormat::Unsupported),
            vk::Format::UNDEFINED
        );
    }

    #[test]
    fn texture_format_maps_to_rgba8_unorm() {
        // AC #1: the portable sampled-texture format maps to the one host format the
        // subset allows, checked against a hand-written vk::Format literal.
        assert_eq!(
            vk_texture_format(TextureFormat::R8G8B8A8Unorm),
            vk::Format::R8G8B8A8_UNORM
        );
    }

    #[test]
    fn blend_key_translates_to_vk_attachment_state() {
        // task-154: the CB_BLEND0_CONTROL → vk::PipelineColorBlendAttachmentState
        // translation, checked against hand-written vk enum literals. The two controls are
        // the live Celeste anchors that anchor the GFX6 field decode.

        // 0x45010501: premultiplied over — SRC=ONE, DST=ONE_MINUS_SRC_ALPHA, ADD.
        // (Note: BlendKey.enable is derived from bit 30 upstream; here it's supplied.)
        let premult = blend_attachment_state(BlendKey {
            enable: true,
            control: 0x4501_0501,
            write_mask: 0xF,
        });
        assert_eq!(premult.blend_enable, 1);
        assert_eq!(premult.src_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(
            premult.dst_color_blend_factor,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );
        assert_eq!(premult.color_blend_op, vk::BlendOp::ADD);
        // SEPARATE_ALPHA_BLEND clear → alpha mirrors color.
        assert_eq!(premult.src_alpha_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(
            premult.dst_alpha_blend_factor,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA
        );
        assert_eq!(premult.alpha_blend_op, vk::BlendOp::ADD);
        assert_eq!(premult.color_write_mask, vk::ColorComponentFlags::RGBA);

        // 0x41040104: additive — SRC=SRC_ALPHA, DST=ONE, ADD.
        let additive = blend_attachment_state(BlendKey {
            enable: true,
            control: 0x4104_0104,
            write_mask: 0xF,
        });
        assert_eq!(additive.blend_enable, 1);
        assert_eq!(additive.src_color_blend_factor, vk::BlendFactor::SRC_ALPHA);
        assert_eq!(additive.dst_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(additive.color_blend_op, vk::BlendOp::ADD);
        assert_eq!(additive.src_alpha_blend_factor, vk::BlendFactor::SRC_ALPHA);
        assert_eq!(additive.dst_alpha_blend_factor, vk::BlendFactor::ONE);

        // SEPARATE_ALPHA_BLEND (bit 29) set with distinct 5/3/5-bit alpha fields:
        // ALPHA_SRCBLEND[20:16]=DST_COLOR(8), ALPHA_COMB_FCN[23:21]=MAX(3),
        // ALPHA_DESTBLEND[28:24]=DST_COLOR(8). Values >= 8 + a comb that only decodes
        // right at shift 21 catch a wrong shift (>>19) or a truncating 3-bit mask.
        let sep = blend_attachment_state(BlendKey {
            enable: true,
            control: 0x6868_0001,
            write_mask: 0xF,
        });
        assert_eq!(sep.src_color_blend_factor, vk::BlendFactor::ONE);
        assert_eq!(sep.src_alpha_blend_factor, vk::BlendFactor::DST_COLOR);
        assert_eq!(sep.dst_alpha_blend_factor, vk::BlendFactor::DST_COLOR);
        assert_eq!(sep.alpha_blend_op, vk::BlendOp::MAX);

        // enable bit clear → disabled attachment, matching the old hardcoded state
        // (blend_enable 0, RGBA write mask). The raw control must be ignored.
        let disabled = blend_attachment_state(BlendKey {
            enable: false,
            control: 0x4501_0501,
            write_mask: 0xF,
        });
        assert_eq!(disabled.blend_enable, 0);
        assert_eq!(disabled.color_write_mask, vk::ColorComponentFlags::RGBA);
        assert_eq!(disabled.src_color_blend_factor, vk::BlendFactor::ZERO);
    }

    /// `CB_TARGET_MASK.TARGET0_ENABLE` reaches the attachment's write mask instead of a
    /// hardcoded RGBA — including with blending DISABLED, where the mask is the only
    /// state that still gates which channels a draw stores.
    #[test]
    fn target_mask_gates_written_colour_channels() {
        // RGB written, ALPHA masked off (bits [3:0] = 0b0111): how a guest keeps the alpha
        // its clear left in a premultiplied-alpha intermediate.
        let rgb_only = blend_attachment_state(BlendKey {
            enable: true,
            control: 0x4501_0501,
            write_mask: 0x7,
        });
        assert_eq!(
            rgb_only.color_write_mask,
            vk::ColorComponentFlags::R | vk::ColorComponentFlags::G | vk::ColorComponentFlags::B
        );
        // Alpha-only, blending off — the disabled path must honour the mask too.
        let alpha_only = blend_attachment_state(BlendKey {
            enable: false,
            control: 0,
            write_mask: 0x8,
        });
        assert_eq!(alpha_only.blend_enable, 0);
        assert_eq!(alpha_only.color_write_mask, vk::ColorComponentFlags::A);
        // A fully-masked target writes nothing.
        let none = blend_attachment_state(BlendKey {
            enable: true,
            control: 0x4501_0501,
            write_mask: 0x0,
        });
        assert_eq!(none.color_write_mask, vk::ColorComponentFlags::empty());
    }

    #[test]
    fn sampler_filter_and_address_map_to_expected_vk() {
        // AC #1: sampler filter/address enums map 1:1 to the hand-written vk values —
        // the portable subset's two filters and two address modes (no anisotropy).
        assert_eq!(vk_filter(SamplerFilter::Nearest), vk::Filter::NEAREST);
        assert_eq!(vk_filter(SamplerFilter::Linear), vk::Filter::LINEAR);
        assert_eq!(
            vk_address_mode(SamplerAddressMode::Repeat),
            vk::SamplerAddressMode::REPEAT
        );
        assert_eq!(
            vk_address_mode(SamplerAddressMode::ClampToEdge),
            vk::SamplerAddressMode::CLAMP_TO_EDGE
        );
    }

    #[test]
    fn unpack_a2b10g10r10_orders_channels_rgba() {
        // task-103: HDR-capable surfaces negotiate A2B10G10R10_UNORM_PACK32, where the
        // little-endian u32 packs R in bits 0-9, G in 10-19, B in 20-29, A in 30-31.
        // Full-scale red (R10=0x3FF, A=3) must unpack to opaque red, not a swapped colour.
        let red = 0x3FFu32 | (0x3 << 30); // R=max, A=max
        let blue = (0x3FFu32 << 20) | (0x3 << 30); // B=max, A=max
        let mut raw = Vec::new();
        raw.extend_from_slice(&red.to_le_bytes());
        raw.extend_from_slice(&blue.to_le_bytes());

        // A2B10G10R10: R in the low field.
        let rgba = unpack_a2_10_10_10(&raw, false);
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "red pixel");
        assert_eq!(&rgba[4..8], &[0, 0, 255, 255], "blue pixel");

        // A2R10G10B10 (r_high): the R and B fields swap, so the same bytes read inverted.
        let swapped = unpack_a2_10_10_10(&raw, true);
        assert_eq!(&swapped[0..4], &[0, 0, 255, 255], "low field is B here");
        assert_eq!(&swapped[4..8], &[255, 0, 0, 255], "high field is R here");
    }

    #[test]
    fn draw_guards_require_every_declared_binding_not_just_one() {
        let pipe = Some(vk::Pipeline::null());
        // The task-184 shape: a pipeline declaring BOTH constant buffers (VS set0/bind2 and
        // PS set0/bind6 — Celeste's bloom-blur passes) with only ONE resolved. Recording it
        // leaves the other descriptor unwritten, which is UB that renders a plausible wrong
        // picture instead of faulting, so the draw must drop.
        assert!(!draw_guards_ok(pipe, 2, 1, 0, 0, 0, 0));
        assert!(draw_guards_ok(pipe, 2, 2, 0, 0, 0, 0));
        // Same contract per vertex-pull stream (task-153): 1 of 3 resolved must not record.
        assert!(!draw_guards_ok(pipe, 0, 0, 3, 1, 0, 0));
        assert!(draw_guards_ok(pipe, 0, 0, 3, 3, 0, 0));
        // Declaring nothing needs nothing; a missing pipeline always drops.
        assert!(draw_guards_ok(pipe, 0, 0, 0, 0, 0, 0));
        assert!(!draw_guards_ok(None, 0, 0, 0, 0, 0, 0));
        // Combined image-samplers COUNT too (task-199): a PS declaring two textures with
        // only one resolved bind must drop, exactly like a half-resolved const buffer —
        // the un-written descriptor would sample garbage and look plausible.
        assert!(!draw_guards_ok(pipe, 0, 0, 0, 0, 1, 0));
        assert!(draw_guards_ok(pipe, 0, 0, 0, 0, 1, 1));
        assert!(!draw_guards_ok(pipe, 0, 0, 0, 0, 2, 1));
        assert!(draw_guards_ok(pipe, 0, 0, 0, 0, 2, 2));
    }

    // ---------------------------------------------------------------------------------
    // RT readback packing (task-181).
    //
    // These start from exactly what `copy_rt_to_host` hands the readback: a TIGHT
    // `w*h*4` linear RGBA8 buffer (the `vkCmdCopyImageToBuffer` region leaves
    // `buffer_row_length`/`buffer_image_height` zero, so the copy is tightly packed to
    // `image_extent`). What they verify is the half that was WRONG — turning those
    // content texels into the guest surface's own bytes — by decoding the result the way
    // the guest/upload path decodes that surface and demanding the content back.
    // ---------------------------------------------------------------------------------

    /// A `w`x`h` RGBA8 pattern where every texel is UNIQUE and never zero: a skew, a
    /// transpose, or a dropped row cannot survive a comparison against it (a flat or
    /// mostly-black pattern would hide exactly the defect under test).
    fn unique_pattern(w: u32, h: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[
                    (x + 1) as u8,
                    (y + 1) as u8,
                    ((x * 7 + y * 13) % 251 + 1) as u8,
                    0xFF,
                ]);
            }
        }
        px
    }

    /// Read texel `(x, y)` out of guest bytes laid out row-major at `pitch` texels/row —
    /// the arithmetic a guest (and the executor's own RT probe) uses on a linear surface.
    /// Written independently of the packing code under test.
    fn guest_texel_linear(bytes: &[u8], pitch: u32, x: u32, y: u32) -> [u8; 4] {
        let off = ((y * pitch + x) * 4) as usize;
        [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]
    }

    #[test]
    fn readback_strides_rows_by_the_guest_pitch_not_the_content_width() {
        // THE task-181 defect, in the shape Celeste has: content narrower than the guest
        // row stride (960 in a 1024 pitch; 10 in a 16 here). Packing at the content width
        // put row y at byte `y*w*4` while every reader indexes `y*pitch*4`, so from row 1
        // on the readback reported texels from the wrong scanline — plausible values,
        // wrong picture.
        let (w, h, pitch) = (10u32, 6u32, 16u32);
        let content = unique_pattern(w, h);
        let layout = guest_surface_layout(w, h, pitch, ps4_core::gpu::Tiling::Linear)
            .expect("a linear padded surface is expressible");
        let guest = pack_guest_surface(&content, w, h, &layout).expect("packs");

        assert_eq!(
            guest.len(),
            (pitch * h * 4) as usize,
            "the packed surface spans the PADDED allocation, not the content"
        );
        for y in 0..h {
            for x in 0..w {
                let want = {
                    let o = ((y * w + x) * 4) as usize;
                    [content[o], content[o + 1], content[o + 2], content[o + 3]]
                };
                assert_eq!(
                    guest_texel_linear(&guest, pitch, x, y),
                    want,
                    "texel ({x},{y}) must be readable at the guest pitch"
                );
            }
        }
        // The padding columns are zeroed, and say so: a reader must not mistake them for
        // rendered content (they are also what a content-width pack would have filled with
        // the NEXT row's texels).
        for y in 0..h {
            for x in w..pitch {
                assert_eq!(
                    guest_texel_linear(&guest, pitch, x, y),
                    [0, 0, 0, 0],
                    "padding texel ({x},{y}) must be zero, not borrowed content"
                );
            }
        }
    }

    #[test]
    fn readback_of_a_tiled_surface_detiles_back_to_the_rendered_content() {
        // The tiled case. A 1D-thin micro-tiled guest surface is NOT row-major: a linear
        // write into one is readable as noise, not as the picture. Round-trip through the
        // UPLOAD path's own `detile` — the decoder that reads this surface everywhere else
        // in the emulator — and demand the exact rendered texels back.
        let (w, h, pitch) = (10u32, 8u32, 16u32);
        let content = unique_pattern(w, h);
        let layout = guest_surface_layout(
            w,
            h,
            pitch,
            // Tile-mode index 1..=7 is 1D-thin (`tile_kind`).
            ps4_core::gpu::Tiling::Tiled { tile_mode_index: 2 },
        )
        .expect("1D-thin is expressible");
        let guest = pack_guest_surface(&content, w, h, &layout).expect("packs");

        // Not row-major: if it were, the tiled path would be silently doing nothing.
        let linear_layout = guest_surface_layout(w, h, pitch, ps4_core::gpu::Tiling::Linear)
            .expect("linear is expressible");
        let as_linear = pack_guest_surface(&content, w, h, &linear_layout).expect("packs");
        assert_ne!(
            guest, as_linear,
            "a tiled pack must actually swizzle — identical bytes would mean the tiling was ignored"
        );

        let decoded = ps4_gnm::cache::tile::detile(&guest, &layout).expect("detiles");
        for y in 0..h {
            for x in 0..w {
                let want = {
                    let o = ((y * w + x) * 4) as usize;
                    [content[o], content[o + 1], content[o + 2], content[o + 3]]
                };
                assert_eq!(
                    guest_texel_linear(&decoded, pitch, x, y),
                    want,
                    "tiled texel ({x},{y}) must survive the pack→detile round trip"
                );
            }
        }
    }

    #[test]
    fn readback_refuses_surfaces_it_cannot_express_rather_than_writing_garbage() {
        // 2D macro-tiling has no re-tiler in this repo, so there is no honest byte to
        // write. Celeste's render targets are tile-mode index 14 — packing them linearly
        // is what let a bright bloom target read back near-black during task-179, so the
        // readback must DECLINE, not approximate.
        for index in [9u32, 13, 14, 31] {
            assert!(
                guest_surface_layout(
                    960,
                    540,
                    1024,
                    ps4_core::gpu::Tiling::Tiled {
                        tile_mode_index: index
                    }
                )
                .is_none(),
                "macro-tiled index {index} must be refused"
            );
        }
        // A mode index that is not even a byte is not a tile mode; refused, not truncated
        // into one that happens to be supported.
        assert!(
            guest_surface_layout(
                8,
                8,
                8,
                ps4_core::gpu::Tiling::Tiled {
                    tile_mode_index: 0x100
                }
            )
            .is_none()
        );
        // A stride narrower than the content it must hold is incoherent — refuse rather
        // than pack rows that overlap each other.
        assert!(guest_surface_layout(16, 8, 8, ps4_core::gpu::Tiling::Linear).is_none());
        // Degenerate extents have no texels to mirror.
        assert!(guest_surface_layout(0, 8, 8, ps4_core::gpu::Tiling::Linear).is_none());
        assert!(guest_surface_layout(8, 0, 8, ps4_core::gpu::Tiling::Linear).is_none());
    }

    #[test]
    fn readback_treats_the_linear_aligned_mode_as_a_pitch_strided_surface() {
        // Tile-mode index 8 is GFX7 linear-aligned: row-major at a padded pitch. Once the
        // rows carry that padding it is byte-identical to the linear-general pack, so the
        // two must agree — a divergence would mean one of them is inventing a layout.
        let (w, h, pitch) = (10u32, 4u32, 16u32);
        let content = unique_pattern(w, h);
        let aligned = guest_surface_layout(
            w,
            h,
            pitch,
            ps4_core::gpu::Tiling::Tiled { tile_mode_index: 8 },
        )
        .expect("linear-aligned is expressible");
        let general = guest_surface_layout(w, h, pitch, ps4_core::gpu::Tiling::Linear)
            .expect("linear is expressible");
        assert_eq!(
            pack_guest_surface(&content, w, h, &aligned).expect("packs"),
            pack_guest_surface(&content, w, h, &general).expect("packs"),
        );
    }

    #[test]
    fn readback_of_an_unpadded_surface_is_the_content_verbatim() {
        // The `pitch == width` case that used to be the ONLY correct one: it must stay a
        // straight copy, so the padded/tiled fixes did not buy correctness by changing the
        // shape that already worked.
        let (w, h) = (8u32, 4u32);
        let content = unique_pattern(w, h);
        let layout = guest_surface_layout(w, h, w, ps4_core::gpu::Tiling::Linear).expect("linear");
        assert_eq!(
            pack_guest_surface(&content, w, h, &layout).expect("packs"),
            content
        );
    }
}
