//! ash implementation of the `ps4-core` `GpuBackend` trait (doc-4 §2, §7 step 1).
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
    BackendCmd, GpuBackend, GpuError, IndexType, PipelineId, PipelineKey, PushConstantRange,
    ResourceDesc, ResourceId, SamplerDesc, ScissorRect, StorageBinding, TargetDesc, TargetId,
    TextureBinding, TextureFormat, ViewportRect,
};
use ps4_core::memory::VirtualMemoryManager;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::commands::DisplayBuffer;
use crate::present_profile::{self, PRESENT};
use crate::vulkan::{ImportedBuf, VulkanContext};

const RES_W: u32 = 1920;
const RES_H: u32 = 1080;

/// Sentinel handed to [`GpuBackend::present`] meaning "the target most recently
/// submitted via [`AshBackend::submit_flip`]". The videoout framebuffer is the
/// only target today, tracked by its `(handle, index)` inside the backend; the
/// opaque [`TargetId`] keeps the trait Vulkan-free without a lossy encoding.
pub const CURRENT_TARGET: TargetId = TargetId(0);

/// The ash present backend. Sole impl of [`GpuBackend`] (doc-4 §2 keeps one impl).
pub struct AshBackend {
    ctx: VulkanContext,
    guest_memory: Arc<RwLock<Box<dyn VirtualMemoryManager>>>,

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
    // The display-side id -> vk buffer map for the resource cache (doc-4 §8, §3).
    // Ids are minted GUEST-SIDE by the `ps4-gnm` ResourceCache and handed in via
    // create_resource / try_import_host_range; the backend only records the vk::Buffer
    // it allocated (or imported) under each id. See `GpuBackend::create_resource` and
    // the `ps4-gnm::cache` module doc for the id-ownership rationale.
    resources: HashMap<u32, CacheBuffer>,
    // The display-side id -> sampled image map (doc-4 §C3/§C4). Ids are minted GUEST-SIDE
    // like `resources`: a `CreateImage` allocates the vk::Image + view + memory under its
    // id, an `UploadImage` stages detiled linear pixels into it, and a `BindTexture` reads
    // the view back to write a combined image-sampler descriptor. Leaked on exit.
    images: HashMap<u32, CacheImage>,
    // The display-side id -> sampler map (doc-4 §C4). A `CreateSampler` records a
    // vk::Sampler under its guest-minted id; a `BindTexture` reads it back. Leaked on exit.
    samplers: HashMap<u32, vk::Sampler>,

    // ---- host-pipeline draw (doc-4 §4, decision-7) ----
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
    // A dedicated fence the whole-submit draw list is submitted with, so the display
    // thread waits on THIS list's completion (not a global `device_wait_idle`) before
    // the present path reads `texture_image`. Created lazily; reset+reused per list.
    draw_fence: Option<vk::Fence>,
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
/// recorded under its guest-minted [`PipelineId`](ps4_core::gpu::PipelineId) (doc-4 §4,
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
}

/// The render pass + framebuffer that target the videoout `texture_image` for a draw.
/// Shared by all pipelines (one fixed color target).
struct EmbeddedTarget {
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,
}

/// A host-visible linear buffer backing one resource-cache entry (doc-4 §8.2 copy
/// path). Held live in `AshBackend::resources` under its guest-minted id; leaked on
/// exit with the rest of the Vulkan state. `mem` is retained so `upload` can map it.
struct CacheBuffer {
    // The buffer is bound as a vertex/index buffer at draw time and kept alive (VRAM
    // stays live until evicted); `mem` is what `upload` maps.
    buffer: vk::Buffer,
    mem: vk::DeviceMemory,
}

/// A device-local sampled image backing one texture-cache entry (doc-4 §C3/§C4). Held live
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

/// The one SSBO a recompiled-VS draw binds (`BindStorageBuffer`), resolved to its cached
/// `vk::Buffer`. The record pass allocates a descriptor set against the bound pipeline's
/// set layout, points `binding` at this buffer, and pushes `num_records` as the fetch
/// clamp before the draw.
struct StorageBind {
    #[allow(dead_code)] // set 0 is the only descriptor set today; kept for parity.
    set: u32,
    binding: u32,
    buffer: vk::Buffer,
    num_records: u32,
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

impl AshBackend {
    pub fn new(
        ctx: VulkanContext,
        guest_memory: Arc<RwLock<Box<dyn VirtualMemoryManager>>>,
    ) -> Self {
        Self {
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
            samplers: HashMap::new(),
            pipelines: HashMap::new(),
            draw_target: None,
            embedded_drawn: false,
            draw_fence: None,
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
    /// (doc-4 §3, decision-7). Runs on the display thread that owns the device.
    ///
    /// A `CreatePipeline` builds a `vk::Pipeline` from the carried SPIR-V and records it
    /// under its guest-minted id; a `BindPipeline { id }` selects it; a `DrawAuto`
    /// records the draw. The whole list is recorded into ONE command buffer and
    /// submitted with a per-list fence — no per-draw `device_wait_idle` — so O(draws)
    /// full-GPU stalls per frame collapse to one wait (doc-4 §3 data-list model). The
    /// target is left in `SHADER_READ_ONLY_OPTIMAL` so the next `present()` blit reads
    /// it. A list with no bound-pipeline draw records nothing (the draw was deferred
    /// upstream).
    pub fn run_command_list(&mut self, cmds: &[BackendCmd]) {
        // First pass: apply the resource-cache + pipeline-create commands (these mutate
        // the backend's own maps, not the command buffer). Resolve the last bound
        // pipeline + draw plus the vertex/index/viewport/scissor state for the render
        // pass this list records.
        let mut bound: Option<vk::Pipeline> = None;
        // The bound pipeline's layout + descriptor-set layout, carried alongside the
        // pipeline so the record pass can bind descriptors / push constants for an SSBO
        // draw. `set_layout` is `Some` only for a recompiled-VS pipeline built with a
        // storage binding; the embedded path has an empty layout and no set.
        let mut bound_layout: Option<vk::PipelineLayout> = None;
        let mut bound_set_layout: Option<vk::DescriptorSetLayout> = None;
        let mut draw: Option<DrawCall> = None;
        let mut vertex_buffers: Vec<VertexBinding> = Vec::new();
        let mut storage_bind: Option<StorageBind> = None;
        let mut texture_bind: Option<TextureBind> = None;
        let mut viewport: Option<ViewportRect> = None;
        let mut scissor: Option<ScissorRect> = None;
        for cmd in cmds {
            match cmd {
                BackendCmd::CreatePipeline {
                    id,
                    vs_spirv,
                    ps_spirv,
                    key,
                    target,
                    storage,
                    push_constants,
                    texture,
                } => {
                    self.create_pipeline(
                        *id,
                        vs_spirv,
                        ps_spirv,
                        key,
                        target,
                        *storage,
                        *push_constants,
                        *texture,
                    );
                }
                &BackendCmd::BindPipeline { id } => {
                    if let Some(p) = self.pipelines.get(&id.0) {
                        bound = Some(p.pipeline);
                        bound_layout = Some(p.layout);
                        bound_set_layout = p.set_layout;
                    } else {
                        bound = None;
                        bound_layout = None;
                        bound_set_layout = None;
                    }
                }
                &BackendCmd::DrawAuto { vertex_count } => {
                    draw = Some(DrawCall::Auto { vertex_count })
                }
                &BackendCmd::BindVertexBuffer { slot, id, stride } => {
                    if let Some(res) = self.resources.get(&id.0) {
                        vertex_buffers.push(VertexBinding {
                            slot,
                            buffer: res.buffer,
                            stride,
                        });
                    }
                }
                &BackendCmd::BindStorageBuffer {
                    set,
                    binding,
                    id,
                    num_records,
                } => {
                    if let Some(res) = self.resources.get(&id.0) {
                        storage_bind = Some(StorageBind {
                            set,
                            binding,
                            buffer: res.buffer,
                            num_records,
                        });
                    }
                }
                &BackendCmd::DrawIndexed {
                    id,
                    index_count,
                    index_type,
                } => {
                    if let Some(res) = self.resources.get(&id.0) {
                        draw = Some(DrawCall::Indexed {
                            buffer: res.buffer,
                            index_count,
                            index_type,
                        });
                    }
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
                    if let (Some(img), Some(&sampler)) = (
                        self.images.get(&image_id.0),
                        self.samplers.get(&sampler_id.0),
                    ) {
                        texture_bind = Some(TextureBind {
                            set,
                            binding,
                            view: img.view,
                            sampler,
                        });
                    }
                }
            }
        }
        let (Some(pipeline), Some(draw)) = (bound, draw) else {
            return;
        };
        if self.draw_target.is_none() {
            return;
        }
        // A storage-buffer draw needs the pipeline layout to bind its descriptor set and
        // push num_records; the layout was recorded alongside the pipeline. Bind is dropped
        // if the layout/set could not be resolved (defensive; a defer never reaches here).
        let pipeline_layout = bound_layout.unwrap_or(vk::PipelineLayout::null());
        // Fence first (needs `&mut self`), then borrow the target immutably for the record.
        let fence = self.ensure_draw_fence();
        let target = self.draw_target.as_ref().unwrap();
        // SAFETY: display thread owns the device; the target/pipeline/fence are live for
        // the process; `texture_image` is the fixed videoout image the present path blits.
        unsafe {
            record_draw_list(
                &self.ctx,
                target,
                pipeline,
                pipeline_layout,
                bound_set_layout,
                &draw,
                &vertex_buffers,
                storage_bind,
                texture_bind,
                viewport,
                scissor,
                fence,
            );
        }
        // The present blit for this flip must read the drawn image, not overwrite it
        // with the guest framebuffer copy.
        self.embedded_drawn = true;
    }

    /// The per-list draw fence, created lazily and reused. Waiting on this instead of a
    /// global `device_wait_idle` is what lets one submit's whole draw list be a single
    /// GPU wait (doc-4 §3), not O(draws) stalls.
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

    /// Execute an `ImportBuffer` command (doc-4 §8.2). The guest-side `ImportProbe`
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

    /// Execute a `FreeResource` command (doc-4 §8): the guest freed/unmapped the range this
    /// resource backed, so destroy its vk allocation — or, for a zero-copy import, revoke
    /// the external-memory buffer so it stops reading the now-freed host pages. The
    /// guest-side cache has already dropped the entry, so this is authoritative teardown.
    ///
    /// **Fence-safe:** the resource may have been bound by the draw list submitted this
    /// frame, whose fence the present path has not yet waited on. Freeing it now would pull
    /// vk memory out from under an in-flight GPU read. So this waits on the in-flight draw
    /// fence (created lazily by `run_command_list`; `None` before the first draw) before
    /// destroying. That is a coarse whole-list wait, acceptable because a free is rare
    /// (guest teardown), not a per-draw hot path.
    ///
    /// An id present in neither the copy-buffer map (`resources`) nor the import map
    /// (`imported` under the `(-1, id)` cache-facing key) is a no-op: the guest side may
    /// evict an entry the backend never materialized (e.g. one whose create was deferred),
    /// or free a range twice.
    fn free_resource(&mut self, id: ResourceId) {
        let cache_buffer = self.resources.remove(&id.0);
        let imported = self.imported.remove(&(-1, id.0));
        if cache_buffer.is_none() && imported.is_none() {
            return;
        }
        // Wait on the in-flight draw list before freeing so no GPU read is live over the
        // resource (fence-safe teardown). `draw_fence` is `None` until the first draw list;
        // if unset there is no in-flight GPU work referencing this resource.
        if let Some(fence) = self.draw_fence {
            // SAFETY: display thread owns the device; the fence is the live per-list fence.
            unsafe {
                self.ctx
                    .device
                    .wait_for_fences(&[fence], true, u64::MAX)
                    .ok();
            }
        }
        // SAFETY: the wait above guarantees no in-flight GPU work references these; the
        // buffers/memory were allocated on this thread and are not aliased elsewhere.
        unsafe {
            if let Some(buf) = cache_buffer {
                self.ctx.device.destroy_buffer(buf.buffer, None);
                self.ctx.device.free_memory(buf.mem, None);
            }
            if let Some(imp) = imported {
                self.ctx.destroy_imported_buffer(&imp);
            }
        }
    }

    /// Build the host pipeline a `CreatePipeline` command names and record it under its
    /// guest-minted id (doc-4 §4, decision-7). The shared render target (render pass +
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
        storage: Option<StorageBinding>,
        push_constants: Option<PushConstantRange>,
        texture: Option<TextureBinding>,
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
        // `storage` selects the layout: `Some` builds a descriptor set + push-constant
        // range with EMPTY vertex input (a recompiled VS fetches via SSBO + gl_VertexIndex);
        // `None` uses `key.vertex_layout` for the vertex-input state (None there = the
        // embedded gl_VertexIndex path, an empty pipeline layout).
        let built = unsafe {
            create_host_pipeline(
                &self.ctx,
                render_pass,
                vs_spirv,
                ps_spirv,
                key.vertex_layout,
                storage,
                push_constants,
                texture,
            )
        };
        self.pipelines.insert(id.0, built);
    }

    /// Create a sampled image under its guest-minted id (doc-4 §C3/§C4). Allocates the
    /// device-local `vk::Image` + view + memory and records `id -> image`; the pixels arrive
    /// separately via [`Self::upload_image`]. A re-create under an id already present is a
    /// no-op — the cache emits one create per image.
    fn create_image(&mut self, id: ResourceId, width: u32, height: u32, format: TextureFormat) {
        if self.images.contains_key(&id.0) {
            return;
        }
        let vk_format = vk_texture_format(format);
        // SAFETY: display thread owns the device; ctx is live.
        let (image, view, mem) = unsafe { self.ctx.create_sampled_image(width, height, vk_format) };
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

    /// Stage detiled linear RGBA `pixels` into the sampled image `id` and leave it
    /// `SHADER_READ_ONLY_OPTIMAL` (doc-4 §C3). Unknown ids (never created) are a no-op.
    fn upload_image(&mut self, id: ResourceId, pixels: &[u8]) {
        if let Some(img) = self.images.get(&id.0) {
            let (image, w, h) = (img.image, img.extent.width, img.extent.height);
            // SAFETY: `image` is the sampled image created under this id; ctx is live.
            unsafe { self.ctx.upload_image(image, w, h, pixels) };
        }
    }

    /// Create a sampler under its guest-minted id from the portable [`SamplerDesc`]
    /// (doc-4 §C4). A re-create under an id already present is a no-op.
    fn create_sampler(&mut self, id: ResourceId, desc: SamplerDesc) {
        if self.samplers.contains_key(&id.0) {
            return;
        }
        // SAFETY: display thread owns the device; ctx is live.
        let sampler = unsafe {
            self.ctx.create_sampler(
                vk_filter(desc.mag_filter),
                vk_filter(desc.min_filter),
                vk_address_mode(desc.address_mode),
            )
        };
        self.samplers.insert(id.0, sampler);
    }

    /// Registers a guest framebuffer and, when eligible, imports it zero-copy.
    /// Relocated verbatim from the `RegisterBuffer` display-loop arm.
    pub fn register_buffer(&mut self, ptr: u64, w: u32, h: u32, handle: i32, index: u32) {
        let key = (handle, index);
        self.buffers.insert(
            key,
            DisplayBuffer {
                guest_ptr: ptr,
                width: w,
                height: h,
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
                    let copy_size = (buf.width * buf.height * 4) as usize;
                    let max_size = (RES_W * RES_H * 4) as usize;
                    let actual_size = std::cmp::min(copy_size, max_size);

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
            record_command_buffer(ctx, image_index, src_buffer, embedded_drawn);

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
        // Id-ownership (doc-4 §3): `id` is minted guest-side by the
        // ps4-gnm ResourceCache; the backend only allocates VRAM under it and records
        // it in `resources`. Linear host-visible buffer for the phase-3.5 copy path.
        // SAFETY: display thread owns the device; ctx is live.
        let (buffer, mem) = unsafe { self.ctx.create_cache_buffer(desc.size) };
        self.resources.insert(id.0, CacheBuffer { buffer, mem });
    }

    fn upload(&mut self, id: ResourceId, offset: u64, bytes: &[u8]) {
        // Copy path (doc-4 §8.2): memcpy the guest bytes into the host-coherent
        // cache buffer. Unknown ids (never created) are a no-op — the cache only
        // uploads to ids it created.
        if let Some(res) = self.resources.get(&id.0) {
            let mem = res.mem;
            // SAFETY: `mem` is the host-visible allocation created under this id.
            unsafe { self.ctx.upload_cache_buffer(mem, offset, bytes) };
        }
    }

    unsafe fn try_import_host_range(
        &mut self,
        id: ResourceId,
        host_ptr: *const u8,
        size: u64,
    ) -> bool {
        // Zero-copy fork (doc-4 §8.2): import the guest range directly under the
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

        ctx.device.cmd_draw(cb, 3, 1, 0, 0);

        ctx.device.cmd_end_render_pass(cb);
        ctx.device.end_command_buffer(cb).unwrap();
    }
}

/// The videoout target format the embedded draw renders into — the fixed
/// `texture_image` format (`R8G8B8A8_UNORM`). Part of the pipeline key on real HW
/// (doc-4 §8); fixed for phase 3.5, so not yet threaded into the cache key.
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
    storage: Option<StorageBinding>,
    push_constants: Option<PushConstantRange>,
    texture: Option<TextureBinding>,
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
        // — a phantom attribute the VS never reads would be an invalid pipeline.
        let vtx_bindings: Vec<vk::VertexInputBindingDescription> = if storage.is_some() {
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
        let vtx_attrs: Vec<vk::VertexInputAttributeDescription> = if storage.is_some() {
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
            topology: vk::PrimitiveTopology::TRIANGLE_LIST,
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
        let color_blend_attachment = [vk::PipelineColorBlendAttachmentState {
            color_write_mask: vk::ColorComponentFlags::RGBA,
            blend_enable: 0,
            ..Default::default()
        }];
        let color_blending = vk::PipelineColorBlendStateCreateInfo {
            s_type: vk::StructureType::PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
            attachment_count: 1,
            p_attachments: color_blend_attachment.as_ptr(),
            ..Default::default()
        };

        // Pipeline layout. The embedded path binds no resources → an empty layout
        // (unchanged). A recompiled VS that fetches through an SSBO needs a STORAGE_BUFFER
        // binding (VERTEX stage) plus a push-constant range (VERTEX stage) for the
        // num_records fetch clamp; a pixel shader that samples a texture needs a
        // COMBINED_IMAGE_SAMPLER binding (FRAGMENT stage). Both live in the one set-0 layout
        // — a pipeline whose SPIR-V declares either must declare it here or the driver faults.
        let mut dsl_bindings: Vec<vk::DescriptorSetLayoutBinding> = Vec::new();
        if let Some(s) = storage {
            dsl_bindings.push(vk::DescriptorSetLayoutBinding {
                binding: s.binding,
                descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::VERTEX,
                ..Default::default()
            });
        }
        if let Some(t) = texture {
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
        }
    }
}

/// Map a Vulkan-free [`VertexFormat`] to the concrete `vk::Format` a vertex-attribute
/// description declares (doc-4 §C4). This is the `vk::*` half of the `dfmt`/`nfmt` →
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
/// (doc-4 §C3). The portability subset samples only `R8G8B8A8_UNORM` this phase.
fn vk_texture_format(format: TextureFormat) -> vk::Format {
    match format {
        TextureFormat::R8G8B8A8Unorm => vk::Format::R8G8B8A8_UNORM,
    }
}

/// Map a Vulkan-free [`SamplerFilter`] to `vk::Filter` (doc-4 §C4).
fn vk_filter(filter: ps4_core::gpu::SamplerFilter) -> vk::Filter {
    use ps4_core::gpu::SamplerFilter;
    match filter {
        SamplerFilter::Nearest => vk::Filter::NEAREST,
        SamplerFilter::Linear => vk::Filter::LINEAR,
    }
}

/// Map a Vulkan-free [`SamplerAddressMode`] to `vk::SamplerAddressMode` (doc-4 §C4).
fn vk_address_mode(mode: ps4_core::gpu::SamplerAddressMode) -> vk::SamplerAddressMode {
    use ps4_core::gpu::SamplerAddressMode;
    match mode {
        SamplerAddressMode::Repeat => vk::SamplerAddressMode::REPEAT,
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

/// Record + submit one submit's draw into the videoout `texture_image` (doc-4 §3).
/// Records the render pass into ONE throwaway command buffer, submits it signalling
/// `fence`, waits on THAT fence (not a global `device_wait_idle`) so the present path —
/// which shares the queue and reads `texture_image` — proceeds only once this list is
/// done, then frees the command buffer. Waiting on a per-list fence is the cheap
/// scaling the per-submit model buys over a per-draw idle. The render pass leaves the
/// image `SHADER_READ_ONLY_OPTIMAL` for the subsequent present blit.
///
/// # Safety
/// `ctx`/`fence` must be live and owned by the display thread; `pipeline`/`target`/the
/// vertex+index buffers are valid.
#[allow(clippy::too_many_arguments)]
unsafe fn record_draw_list(
    ctx: &VulkanContext,
    target: &EmbeddedTarget,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    set_layout: Option<vk::DescriptorSetLayout>,
    draw: &DrawCall,
    vertex_buffers: &[VertexBinding],
    storage_bind: Option<StorageBind>,
    texture_bind: Option<TextureBind>,
    viewport: Option<ViewportRect>,
    scissor: Option<ScissorRect>,
    fence: vk::Fence,
) {
    unsafe {
        let alloc = vk::CommandBufferAllocateInfo {
            s_type: vk::StructureType::COMMAND_BUFFER_ALLOCATE_INFO,
            command_pool: ctx.command_pool,
            level: vk::CommandBufferLevel::PRIMARY,
            command_buffer_count: 1,
            ..Default::default()
        };
        let cb = ctx.device.allocate_command_buffers(&alloc).unwrap()[0];

        let begin = vk::CommandBufferBeginInfo {
            s_type: vk::StructureType::COMMAND_BUFFER_BEGIN_INFO,
            flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
            ..Default::default()
        };
        ctx.device.begin_command_buffer(cb, &begin).unwrap();

        let clear = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        }];
        let rp_begin = vk::RenderPassBeginInfo {
            s_type: vk::StructureType::RENDER_PASS_BEGIN_INFO,
            render_pass: target.render_pass,
            framebuffer: target.framebuffer,
            render_area: vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: RES_W,
                    height: RES_H,
                },
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
        let has_binding = storage_bind.is_some() || texture_bind.is_some();
        let desc_pool: Option<vk::DescriptorPool> = match (has_binding, set_layout) {
            (true, Some(dsl)) => {
                let mut sizes: Vec<vk::DescriptorPoolSize> = Vec::new();
                if storage_bind.is_some() {
                    sizes.push(vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::STORAGE_BUFFER,
                        descriptor_count: 1,
                    });
                }
                if texture_bind.is_some() {
                    sizes.push(vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        descriptor_count: 1,
                    });
                }
                let pool_info = vk::DescriptorPoolCreateInfo {
                    s_type: vk::StructureType::DESCRIPTOR_POOL_CREATE_INFO,
                    max_sets: 1,
                    pool_size_count: sizes.len() as u32,
                    p_pool_sizes: sizes.as_ptr(),
                    ..Default::default()
                };
                let pool = ctx.device.create_descriptor_pool(&pool_info, None).unwrap();
                let layouts = [dsl];
                let alloc_info = vk::DescriptorSetAllocateInfo {
                    s_type: vk::StructureType::DESCRIPTOR_SET_ALLOCATE_INFO,
                    descriptor_pool: pool,
                    descriptor_set_count: 1,
                    p_set_layouts: layouts.as_ptr(),
                    ..Default::default()
                };
                let dset = ctx.device.allocate_descriptor_sets(&alloc_info).unwrap()[0];

                // The write structs reference these infos by raw pointer, so both must
                // outlive `update_descriptor_sets`.
                let buf_info = storage_bind.as_ref().map(|sb| {
                    [vk::DescriptorBufferInfo {
                        buffer: sb.buffer,
                        offset: 0,
                        range: vk::WHOLE_SIZE,
                    }]
                });
                let img_info = texture_bind.as_ref().map(|tb| {
                    [vk::DescriptorImageInfo {
                        sampler: tb.sampler,
                        image_view: tb.view,
                        image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    }]
                });
                let mut writes: Vec<vk::WriteDescriptorSet> = Vec::new();
                if let (Some(sb), Some(bi)) = (storage_bind.as_ref(), buf_info.as_ref()) {
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
                if let (Some(tb), Some(ii)) = (texture_bind.as_ref(), img_info.as_ref()) {
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
                // A vertex-pull VS reads num_records from a push constant; a texture-only
                // draw declares none, so push only when a storage bind is present.
                if let Some(sb) = storage_bind.as_ref() {
                    ctx.device.cmd_push_constants(
                        cb,
                        pipeline_layout,
                        vk::ShaderStageFlags::VERTEX,
                        0,
                        &sb.num_records.to_le_bytes(),
                    );
                }
                Some(pool)
            }
            _ => None,
        };

        // Dynamic viewport/scissor: use the register-derived rect, defaulting to the full
        // videoout target when the draw programmed none. A negative viewport height is the
        // portable Vulkan Y-flip (decision-3) — passed straight through. A zero-area rect is
        // treated as "unprogrammed": the embedded fullscreen corpus sets no PA_CL_VPORT /
        // scissor registers, so its derived rect is all-zeros; drawing into a 0x0 viewport
        // rasterizes nothing, so it too falls back to the full videoout target.
        let full_vp = ViewportRect {
            x: 0.0,
            y: 0.0,
            width: RES_W as f32,
            height: RES_H as f32,
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
            width: RES_W,
            height: RES_H,
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
        ctx.device.end_command_buffer(cb).unwrap();

        let cbs = [cb];
        let submit = vk::SubmitInfo {
            s_type: vk::StructureType::SUBMIT_INFO,
            command_buffer_count: 1,
            p_command_buffers: cbs.as_ptr(),
            ..Default::default()
        };
        // Reset the reused fence, submit signalling it, and block on it — so only THIS
        // list's completion is waited on before the shared-queue present path records.
        let fences = [fence];
        ctx.device.reset_fences(&fences).unwrap();
        ctx.device
            .queue_submit(ctx.queue, &[submit], fence)
            .unwrap();
        // Bounded wait so a hung/faulted GPU submit cannot deadlock the display thread; on
        // timeout the present path proceeds (it reads whatever the target holds), never a
        // deadlock. The shared-queue present records only after this returns.
        let _ = ctx
            .device
            .wait_for_fences(&fences, true, DRAW_FENCE_TIMEOUT_NS);
        ctx.device.free_command_buffers(ctx.command_pool, &cbs);
        // The per-list descriptor pool is done once the fence is signalled (or timed out):
        // free it here so a storage-buffer draw leaks no pool across frames.
        if let Some(pool) = desc_pool {
            ctx.device.destroy_descriptor_pool(pool, None);
        }
    }
}

/// Bound on the per-list draw-fence wait in [`record_draw_list`]. A finite timeout so a
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
        let cb = ctx.device.allocate_command_buffers(&alloc).unwrap()[0];
        let begin = vk::CommandBufferBeginInfo {
            s_type: vk::StructureType::COMMAND_BUFFER_BEGIN_INFO,
            flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
            ..Default::default()
        };
        ctx.device.begin_command_buffer(cb, &begin).unwrap();

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

        ctx.device.end_command_buffer(cb).unwrap();
        let fence = ctx
            .device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .unwrap();
        let cbs = [cb];
        let submit = vk::SubmitInfo {
            s_type: vk::StructureType::SUBMIT_INFO,
            command_buffer_count: 1,
            p_command_buffers: cbs.as_ptr(),
            ..Default::default()
        };
        ctx.device
            .queue_submit(ctx.queue, &[submit], fence)
            .unwrap();
        ctx.device
            .wait_for_fences(&[fence], true, u64::MAX)
            .unwrap();
        ctx.device.destroy_fence(fence, None);
        ctx.device.free_command_buffers(ctx.command_pool, &[cb]);

        let ptr = ctx
            .device
            .map_memory(mem, 0, size, vk::MemoryMapFlags::empty())
            .unwrap() as *const u8;
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
        unpack_a2_10_10_10, vk_address_mode, vk_filter, vk_texture_format, vk_vertex_format,
    };
    use ash::vk;
    use ps4_core::gpu::{SamplerAddressMode, SamplerFilter, TextureFormat, VertexFormat};

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
}
