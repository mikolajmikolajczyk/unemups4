//! GPU backend seam (doc-4 §2).
//!
//! A narrow trait capturing *what* the command processor asks the GPU to do,
//! never *how* ash/Vulkan does it. Kept Vulkan-free so `ps4-gnm` (the future PM4
//! command processor) can target it without ever naming an `ash::vk` type, and so
//! a native Metal backend can later be an alternative impl rather than a rewrite.
//!
//! Only the present + zero-copy-import surface is implemented today (phase 1).
//! `create_target`/`create_resource`/`upload` are stubs; draw/bind/pipeline/sync
//! methods are deliberately absent and grow one-per-phase (doc-4 §2 DEFER, §(b)).

/// Opaque, backend-owned handle for a color/render target (including the videoout
/// framebuffer). The command processor holds these; it never holds `vk::*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TargetId(pub u32);

/// Opaque, backend-owned handle for a cached buffer/texture (doc-4 §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceId(pub u32);

/// Opaque handle naming a host graphics pipeline across the display-thread channel
/// (doc-4 §4). Like [`ResourceId`], it is minted **guest-side** from a monotonic
/// counter — a fire-and-forget `BackendCmd` cannot round-trip a backend-minted id back
/// (doc-4 §3). The guest-side pipeline cache assigns one id per distinct
/// [`PipelineKey`]; the display thread records `id -> vk::Pipeline` in its own map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PipelineId(pub u32);

/// Host-facing color format of a render target (doc-4 §C3). Vulkan-free: an
/// enumerated identity the backend maps to a concrete `vk::Format`, never a
/// `vk::*` value. Grows as the corpus needs more formats; anything the decoder
/// does not recognize stays [`ColorFormat::Unsupported`] so the draw defers
/// cleanly rather than guessing a format (AC #3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ColorFormat {
    /// 8-8-8-8 unsigned-normalized (the videoout framebuffer format today).
    #[default]
    B8G8R8A8Unorm,
    /// 8-8-8-8 unsigned-normalized, RGBA channel order.
    R8G8B8A8Unorm,
    /// A recognized register value the decoder does not yet map to a host format.
    Unsupported,
}

/// The tiling/swizzle layout carried on a render target (doc-4 §C3/§C9). The first
/// implementation forces surfaces **linear + uncompressed** (§C9 correctness-first),
/// but the field is carried from day one so the later detile/decompress step has a
/// place to key on without reshaping [`TargetDesc`]. Only the mode is modeled; the
/// GCN micro/macro-tiling math is deferred (§C3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Tiling {
    /// Row-major, no swizzle — the upload path is a no-op detile (§C3).
    #[default]
    Linear,
    /// A GCN tiled/swizzled surface (`ARRAY_MODE != LINEAR`). Carried per §C3 even
    /// while the first implementation forces linear; the tile-mode index the guest
    /// programmed is retained so the deferred detile step can consume it.
    Tiled { tile_mode_index: u32 },
}

/// Description of a render target to create (doc-4 §5/§C3). Derived at draw time from
/// the shadow `CB_COLOR0_*` registers and, for a target that aliases the videoout
/// framebuffer, the registered display-buffer geometry. Plain data; the backend maps
/// `format` to a `vk::Format`. Grows per phase (MRT>1, depth) as the corpus needs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetDesc {
    pub width: u32,
    pub height: u32,
    /// Row pitch in pixels (from `CB_COLOR0_PITCH`), or `width` when it aliases the
    /// videoout framebuffer and no separate pitch was programmed.
    pub pitch: u32,
    /// Host color format (from `CB_COLOR0_INFO`).
    pub format: ColorFormat,
    /// Tiling/compression layout (from `CB_COLOR0_ATTRIB`/`INFO`, §C3/§C9).
    pub tiling: Tiling,
}

/// Description of a cached resource to create. Grows per phase; a placeholder today.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceDesc {
    pub size: u64,
}

/// A storage-buffer (SSBO) binding a recompiled vertex shader fetches its vertices
/// through (doc-4 §C4). A GCN passthrough VS reads no vertex-input: it fetches each
/// vertex from an SSBO at `(set, binding)` indexed by `gl_VertexIndex`, so the host
/// pipeline needs a descriptor-set layout with one `STORAGE_BUFFER` binding rather than
/// vertex-input attributes. `stride` is the per-element byte stride the recompiler baked
/// into the module's `OpTypeRuntimeArray` (16 for one `vec4` per vertex); the draw path
/// rejects a bound V# whose stride disagrees. `None` on `CreatePipeline` for an embedded
/// shader, which fetches nothing and uses the empty-layout `gl_VertexIndex` path.
/// Vulkan-free plain data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StorageBinding {
    /// Descriptor-set index the SSBO is bound at.
    pub set: u32,
    /// Binding index within the set.
    pub binding: u32,
    /// Per-element byte stride baked into the recompiled module (16 = one `vec4`).
    pub stride: u32,
}

/// The push-constant range a recompiled vertex shader reads its fetch clamp
/// (`num_records`) from (doc-4 §C4). A GCN passthrough VS clamps every fetch index to
/// this value; the host pipeline must declare a matching push-constant range and the
/// draw path must supply the V#'s `num_records` at draw time (a zero/missing value
/// silently clamps every vertex to element 0). `NumRecords` is the only role today, so
/// the range is kept implicit — offset/size describe the single `uint` field. `None` on
/// `CreatePipeline` for an embedded shader. Vulkan-free plain data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PushConstantRange {
    /// Byte offset of the field within the push-constant block.
    pub offset: u32,
    /// Field size in bytes (`4` for the `num_records` `uint`).
    pub size: u32,
}

/// Host-facing pixel format of a sampled texture (doc-4 §C3/§C4). Kept deliberately
/// narrow: the portability subset (decision-3, MoltenVK/Metal) samples only
/// uncompressed `R8G8B8A8_UNORM` this phase — no BCn/ASTC, no float/depth formats. The
/// T#-derived `dfmt`/`nfmt` fold into this once the register decode lands; today it names
/// the one format the detiled-linear-bytes upload path produces. Vulkan-free identity the
/// backend maps to a concrete `vk::Format`, mirroring [`ColorFormat`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextureFormat {
    /// 8-8-8-8 unsigned-normalized, RGBA channel order — the detiled linear texel order.
    #[default]
    R8G8B8A8Unorm,
}

/// The minification/magnification filter a sampler applies (doc-4 §C4). Vulkan-free
/// identity the backend maps to `vk::Filter`. The portability subset keeps this to the
/// two core filters (no anisotropy, decision-3); the S#-derived value fills this in once
/// the sampler decode lands. Fixed to [`SamplerFilter::Linear`] for the hardcoded path now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SamplerFilter {
    /// Nearest-texel sampling (`VK_FILTER_NEAREST`).
    Nearest,
    /// Bilinear sampling (`VK_FILTER_LINEAR`).
    #[default]
    Linear,
}

/// The out-of-`[0,1]` address behaviour a sampler applies per axis (doc-4 §C4).
/// Vulkan-free identity the backend maps to `vk::SamplerAddressMode`. The S#-derived
/// value fills this in once the sampler decode lands; fixed to
/// [`SamplerAddressMode::Repeat`] now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SamplerAddressMode {
    /// Wrap (`VK_SAMPLER_ADDRESS_MODE_REPEAT`).
    #[default]
    Repeat,
    /// Clamp to the edge texel (`VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE`).
    ClampToEdge,
}

/// Description of a sampler to create (doc-4 §C4). Plain data derived from an S# once the
/// sampler decode lands; the current path supplies fixed portable defaults (linear filter,
/// repeat addressing, no anisotropy, no mips). The backend maps it to a `vk::Sampler`.
/// Vulkan-free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SamplerDesc {
    /// Magnification filter.
    pub mag_filter: SamplerFilter,
    /// Minification filter.
    pub min_filter: SamplerFilter,
    /// Address mode on all axes (U/V/W share one mode this phase).
    pub address_mode: SamplerAddressMode,
}

/// One combined image-sampler binding a pipeline declares so a pixel shader can sample a
/// texture (doc-4 §C4). Carried on [`BackendCmd::CreatePipeline`] alongside
/// [`StorageBinding`]: when `Some`, the backend adds a `COMBINED_IMAGE_SAMPLER`
/// descriptor at `(set, binding)`, `FRAGMENT` stage, to the pipeline's set-0 layout, and
/// the record pass writes the bound image view + sampler into the allocated set. `None`
/// for a pipeline that samples nothing (the embedded / vertex-pull paths). The portability
/// subset uses the standard combined image-sampler path only (decision-3). Vulkan-free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextureBinding {
    /// Descriptor-set index the combined image-sampler is bound at.
    pub set: u32,
    /// Binding index within the set.
    pub binding: u32,
}

/// The pipeline-relevant snapshot of GPU state a draw derives and hands the backend
/// to get-or-create a host pipeline (doc-4 §4/§5). It carries a shader **identity**
/// (a 64-bit hash per stage, from the `ShaderRef`), the vertex-input layout, the RT
/// format and the blend/depth bits — **not** a hardcoded pipeline handle (doc-4 §4:
/// "PipelineKey must not hardcode"), so phase 4's arbitrary shaders have something to
/// key on and the backend caches by value. Plain data, Vulkan-free.
///
/// `Hash`/`Eq` make it the backend's cache key: two draws that agree on every field
/// name the same host pipeline (AC #2 — the key changes iff a key-relevant register
/// changed). Fields grow per phase (MRT>1, more blend/depth detail); adding one is a
/// cache-key change, by design.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PipelineKey {
    /// Vertex-shader identity (a stable hash of the bound `ShaderRef`), 0 when unbound.
    pub vs_hash: u64,
    /// Pixel-shader identity (a stable hash of the bound `ShaderRef`), 0 when unbound.
    pub ps_hash: u64,
    /// Vertex-input layout the pipeline is built against. `None` for the embedded
    /// fullscreen-quad draw (it reads `gl_VertexIndex`, no vertex buffer). The
    /// register-derived layout (from the vertex-buffer V#s) fills this in.
    pub vertex_layout: Option<VertexLayout>,
    /// Color-target format the pipeline renders into (from `CB_COLOR0_INFO`).
    pub color_format: ColorFormat,
    /// Blend enable/equation bits (from `CB_BLEND0_CONTROL`/`CB_COLOR_CONTROL`).
    pub blend: BlendKey,
    /// Depth test/write bits (from `DB_DEPTH_CONTROL`/`DB_Z_INFO`).
    pub depth: DepthKey,
}

/// The host vertex-attribute format a derived attribute carries (doc-4 §C4). A
/// Vulkan-free identity the backend maps 1:1 to a concrete `vk::Format`, mirroring how
/// [`ColorFormat`] keeps `vk::*` out of the key. Each variant names the `(dfmt, nfmt)`
/// V# combination the fetch produces; the `ps4-gnm` derivation folds its typed
/// `DataFormat`/`NumFormat` into one of these. A combination the table does not model
/// stays [`VertexFormat::Unsupported`] so the draw defers cleanly (AC #1 defer path)
/// rather than the backend guessing a format that would mismatch the SPIR-V input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VertexFormat {
    /// One 32-bit float (`_32` × FLOAT) → `R32_SFLOAT`.
    R32Sfloat,
    /// Two 32-bit floats (`_32_32` × FLOAT) → `R32G32_SFLOAT`.
    R32G32Sfloat,
    /// Three 32-bit floats (`_32_32_32` × FLOAT) → `R32G32B32_SFLOAT`.
    R32G32B32Sfloat,
    /// Four 32-bit floats (`_32_32_32_32` × FLOAT) → `R32G32B32A32_SFLOAT` (the corpus
    /// vec4 position the passthrough VS fetches).
    R32G32B32A32Sfloat,
    /// One 32-bit unsigned int (`_32` × UINT) → `R32_UINT`.
    R32Uint,
    /// Four 32-bit unsigned ints (`_32_32_32_32` × UINT) → `R32G32B32A32_UINT`.
    R32G32B32A32Uint,
    /// One 32-bit signed int (`_32` × SINT) → `R32_SINT`.
    R32Sint,
    /// Four 32-bit signed ints (`_32_32_32_32` × SINT) → `R32G32B32A32_SINT`.
    R32G32B32A32Sint,
    /// Four 8-bit unsigned-normalized (`_8_8_8_8` × UNORM) → `R8G8B8A8_UNORM` (packed
    /// vertex colors).
    R8G8B8A8Unorm,
    /// Two 16-bit unsigned-normalized (`_16_16` × UNORM) → `R16G16_UNORM` (packed UVs).
    R16G16Unorm,
    /// A `(dfmt, nfmt)` combination this table does not model — the draw defers rather
    /// than the backend picking a wrong `vk::Format`.
    #[default]
    Unsupported,
}

/// Maximum vertex attributes (and, one-per-buffer, bindings) a [`VertexLayout`] carries
/// inline. The key stays `Copy`/`Hash` (no heap), so it hashes into [`PipelineKey`] as
/// before; the corpus draws use a small handful of attributes, well under this cap. A
/// draw exceeding it defers cleanly at derivation, never overruns.
pub const MAX_VERTEX_ATTRIBUTES: usize = 16;

/// One vertex attribute the pipeline declares (doc-4 §C4): its shader `location`, the
/// vertex-buffer `binding` it fetches from, the host `format`, and its byte `offset`
/// within that binding's element. Derived from a vertex-buffer V#; the backend turns it
/// into a `vk::VertexInputAttributeDescription`. Vulkan-free plain data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VertexAttr {
    /// Shader input location (matches the SPIR-V `location` decoration).
    pub location: u32,
    /// Vertex-buffer binding slot this attribute fetches from — the same slot the
    /// executor binds the buffer at via `BindVertexBuffer`.
    pub binding: u32,
    /// Host attribute format (mapped from the V# `dfmt`/`nfmt`).
    pub format: VertexFormat,
    /// Byte offset of this attribute within its binding's per-vertex element.
    pub offset: u32,
}

/// One vertex-buffer binding the pipeline declares (doc-4 §C4): the `binding` slot and
/// its per-vertex `stride`. One per referenced vertex buffer, matching the slots the
/// executor emits `BindVertexBuffer` at. The backend turns it into a
/// `vk::VertexInputBindingDescription`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VertexBinding {
    /// Binding slot (matches [`VertexAttr::binding`] and the `BindVertexBuffer` slot).
    pub binding: u32,
    /// Per-vertex stride in bytes (from the V#).
    pub stride: u32,
}

/// Vertex-input layout a pipeline is built against (doc-4 §4/§C4). The register-derived
/// per-attribute format/offset and per-binding stride the backend declares its vertex
/// input from — not a hardcoded vec4. The embedded fullscreen-quad draw reads
/// `gl_VertexIndex` and carries `None` (empty vertex input, unchanged).
///
/// Inline, fixed-capacity arrays keep the layout `Copy`/`Hash` so it still hashes into
/// [`PipelineKey`]; only the first `attribute_count`/`binding_count` entries are live.
/// A layout change re-keys the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VertexLayout {
    /// Number of live entries in `attributes`.
    pub attribute_count: u32,
    /// The declared attributes (only `attribute_count` are live).
    pub attributes: [VertexAttr; MAX_VERTEX_ATTRIBUTES],
    /// Number of live entries in `bindings`.
    pub binding_count: u32,
    /// The declared bindings, one per referenced buffer (only `binding_count` are live).
    pub bindings: [VertexBinding; MAX_VERTEX_ATTRIBUTES],
}

impl Default for VertexLayout {
    fn default() -> Self {
        VertexLayout {
            attribute_count: 0,
            attributes: [VertexAttr::default(); MAX_VERTEX_ATTRIBUTES],
            binding_count: 0,
            bindings: [VertexBinding::default(); MAX_VERTEX_ATTRIBUTES],
        }
    }
}

impl VertexLayout {
    /// The live attribute slice (the first `attribute_count` entries).
    pub fn attributes(&self) -> &[VertexAttr] {
        &self.attributes[..self.attribute_count as usize]
    }

    /// The live binding slice (the first `binding_count` entries).
    pub fn bindings(&self) -> &[VertexBinding] {
        &self.bindings[..self.binding_count as usize]
    }
}

/// The blend bits a draw snapshots into its [`PipelineKey`] (doc-4 §5). Modeled as the
/// raw `CB_BLEND0_CONTROL` word plus the global-enable bit from `CB_COLOR_CONTROL`,
/// carried verbatim so a blend-state change re-keys the pipeline without the decoder
/// having to enumerate every factor this phase (that detail grows as the corpus needs
/// it). MRT>1 is out of scope, so only MRT0 is carried.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct BlendKey {
    /// Whether blending is enabled for MRT0 (`CB_BLEND0_CONTROL.ENABLE`).
    pub enable: bool,
    /// The raw `CB_BLEND0_CONTROL` register value (factors/equation), 0 when unset.
    pub control: u32,
}

/// The depth bits a draw snapshots into its [`PipelineKey`] (doc-4 §5). Depth presence
/// derives from `DB_DEPTH_CONTROL` (test/write enables) and `DB_Z_INFO` (a programmed
/// depth surface); the raw control word is carried so a depth-state change re-keys the
/// pipeline. HTILE is forced off (§C9), so no compression metadata is carried.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DepthKey {
    /// Whether a depth surface is present + tested (from `DB_DEPTH_CONTROL`/`DB_Z_INFO`).
    pub enable: bool,
    /// The raw `DB_DEPTH_CONTROL` register value (compare/write bits), 0 when unset.
    pub control: u32,
}

/// The element width of a bound index buffer (`IT_INDEX_TYPE`, doc-4 §5). GCN encodes
/// 16- and 32-bit indices; the backend maps this to a `vk::IndexType`. Plain data —
/// Vulkan-free, so the executor never names `vk::IndexType`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IndexType {
    /// 16-bit indices (`VGT_INDEX_16`).
    #[default]
    U16,
    /// 32-bit indices (`VGT_INDEX_32`).
    U32,
}

/// A screen-space viewport a draw sets dynamically (`vkCmdSetViewport`, doc-4 §5).
/// Plain data derived from `PA_CL_VPORT_*`; the negative-height Y-flip convention is
/// carried in `height`'s sign (portable Vulkan, decision-3). All `vk::` stays in
/// `ps4-gpu`, so this is a bare rect of floats.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A screen scissor rect a draw sets dynamically (`vkCmdSetScissor`, doc-4 §5). Plain
/// data derived from `PA_SC_SCREEN_SCISSOR_*`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScissorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Errors surfaced across the backend seam. Grows as real error cases appear.
#[derive(thiserror::Error, Debug)]
pub enum GpuError {
    #[error("gpu backend present failed: {0}")]
    Present(String),
}

/// The GPU surface the command processor drives. One impl per backend API
/// (`AshBackend` today; a Metal impl could satisfy the same trait later).
///
/// Deliberately coarse: it speaks in PS4/present concepts ("present a target",
/// "import a guest range"), not raw Vulkan verbs. All command-buffer recording,
/// barriers, render passes and swapchain handling stay inside the backend impl.
///
/// No `Send` bound yet: today the sole impl (`AshBackend`) lives on the display
/// thread that owns the Vulkan device and never crosses a thread boundary. The
/// `Send` bound the doc-4 §2 sketch shows belongs with the §3 channel-crossing
/// executor design and is added when that lands, not speculatively now (§(b)).
pub trait GpuBackend {
    // ---- presentation (phase 1: implemented, relocated from the display loop) ----

    /// Present the given host target to the display (the softgpu framebuffer today).
    fn present(&mut self, target: TargetId) -> Result<(), GpuError>;

    // ---- resource cache backing (phase 3.5+, doc-4 §8) ----

    /// Create a render target. Stub until the resource cache lands (doc-4 §8).
    fn create_target(&mut self, desc: &TargetDesc) -> TargetId;

    /// Create host VRAM for a cached resource under the caller-supplied `id`.
    ///
    /// **Id ownership (doc-4 §3 channel model):** the
    /// [`ResourceId`] is minted **guest-side** by the `ps4-gnm` `ResourceCache`, not
    /// by the backend. The cache runs on the guest thread (only a `&dyn PresentSink`
    /// there), while the sole `GpuBackend` lives on the display thread across a
    /// one-way channel — a fire-and-forget `BackendCmd` cannot round-trip a
    /// backend-minted id back. So the cache allocates the id from its own monotonic
    /// counter and hands it in here; the backend records `id -> vk::Buffer` in its
    /// own map. Any future `BackendCmd` variants that create or upload buffers
    /// MUST carry this guest-minted id for the same reason. See the `ps4-gnm::cache`
    /// module doc for the full rationale.
    fn create_resource(&mut self, id: ResourceId, desc: &ResourceDesc);

    /// Upload host bytes into a cached resource. Stub until the cache lands (doc-4 §8).
    fn upload(&mut self, id: ResourceId, offset: u64, bytes: &[u8]);

    /// Optional zero-copy import of an identity-mapped guest range under the
    /// caller-supplied `id` (minted guest-side, see [`Self::create_resource`]).
    ///
    /// Returns `true` when the range was imported zero-copy (`id` now names the
    /// imported buffer); `false` when the backend/range can't import (MoltenVK,
    /// unaligned) so the caller falls back to `create_resource` + `upload` — making
    /// zero-copy vs copy a single seam (doc-4 §8.2).
    ///
    /// # Safety
    /// `host_ptr` must point to at least `size` bytes that stay mapped and valid for
    /// the lifetime of the imported resource (the identity-mapped guest range lives
    /// for the whole run). The import must be released before that memory is.
    unsafe fn try_import_host_range(
        &mut self,
        id: ResourceId,
        host_ptr: *const u8,
        size: u64,
    ) -> bool;
}

/// A backend-agnostic, **Vulkan-free** GPU command the executor emits for the
/// display thread to replay against the real backend (doc-4 §3: the executor runs
/// on the guest thread and never touches the device, so a draw crosses the channel
/// as *data*, not as direct ash calls). Phase 3.5 needs the embedded fullscreen-quad
/// draw ops plus the resource-cache buffer ops; the enum grows one variant per phase
/// exactly like [`GpuBackend`]'s method list. It names no `ash::vk` type, so
/// `ps4-gnm` can build and ship a `Vec<BackendCmd>` while staying Vulkan-free.
///
/// **Not `Copy`.** `UploadBuffer`/`UploadImage` carry their pixel/byte snapshot as an
/// `Arc<[u8]>` and `CreatePipeline` carries the recompiled SPIR-V as `Arc<[u32]>`, so the guest-thread
/// side can hand a snapshot across the one-way channel without a return value; that
/// reference-counted payload makes the enum `Clone` but not `Copy`. Match on
/// `&BackendCmd` rather than dereferencing.
///
/// **Not `Eq`.** [`CreatePipeline`] carries a [`TargetDesc`], whose viewport-adjacent
/// state is only `PartialEq`; the enum therefore derives `PartialEq` only. Tests
/// compare with `assert_eq!`, which needs no `Eq`.
///
/// [`CreatePipeline`]: BackendCmd::CreatePipeline
#[derive(Clone, Debug, PartialEq)]
pub enum BackendCmd {
    /// Create a host graphics pipeline from recompiled/embedded SPIR-V under the
    /// guest-minted `id` (doc-4 §4, decision-7). Emitted **once per distinct pipeline**
    /// — a guest-side cache keyed by [`PipelineKey`] mints the id and ships this only on
    /// a miss, so steady-state draws carry only [`BindPipeline`](Self::BindPipeline).
    /// The display thread builds the `vk::Pipeline` from the SPIR-V and records
    /// `id -> vk::Pipeline`; the executor names no `vk::*` type (SPIR-V crosses as
    /// `Arc<[u32]>`). `key`/`target` supply the pipeline-state the build keys on (RT
    /// format, blend/depth, vertex layout), grown one-per-milestone with the state model.
    CreatePipeline {
        /// Guest-minted [`PipelineId`] the built pipeline is recorded under.
        id: PipelineId,
        /// Recompiled/embedded vertex-shader SPIR-V (the `HostShader.spirv` payload).
        vs_spirv: std::sync::Arc<[u32]>,
        /// Recompiled/embedded pixel-shader SPIR-V.
        ps_spirv: std::sync::Arc<[u32]>,
        /// The pipeline-state key the build derives blend/depth/vertex-layout from.
        /// Boxed: the inline vertex-attribute layout makes [`PipelineKey`] the largest
        /// field by far, so heap-indirecting it here keeps `BackendCmd`'s variants a
        /// uniform size (the guest-side cache still holds an unboxed `Copy` key).
        key: Box<PipelineKey>,
        /// The color target the pipeline renders into (RT format, dimensions).
        target: TargetDesc,
        /// The SSBO binding a recompiled VS fetches vertices through, `None` for an
        /// embedded shader. When `Some`, the backend builds a descriptor-set layout with
        /// one `STORAGE_BUFFER` binding and an empty vertex-input state (the VS consumes
        /// no vertex-input); when `None`, the vertex-input path in `key` drives the layout.
        storage: Option<StorageBinding>,
        /// The push-constant range a recompiled VS reads its `num_records` fetch clamp
        /// from, `None` for an embedded shader. When `Some`, the backend declares a
        /// matching push-constant range on the pipeline layout.
        push_constants: Option<PushConstantRange>,
        /// The combined image-sampler binding a pixel shader samples a texture through,
        /// `None` when the pipeline samples nothing. When `Some`, the backend adds a
        /// `COMBINED_IMAGE_SAMPLER` descriptor at `(set, binding)` (FRAGMENT stage) to the
        /// set-0 layout so a later [`BindTexture`](Self::BindTexture) can point it at an
        /// image + sampler.
        texture: Option<TextureBinding>,
    },
    /// Bind the host pipeline previously created under `id` (doc-4 §4, decision-7). The
    /// SPIR-V analogue of the old embedded-only bind: steady-state draws ship only this
    /// small id, never the SPIR-V again. The display thread looks `id` up in its
    /// `id -> vk::Pipeline` map.
    BindPipeline {
        /// Guest-minted [`PipelineId`] naming a pipeline a prior [`CreatePipeline`] built.
        id: PipelineId,
    },
    /// A non-indexed draw of `vertex_count` vertices (`IT_DRAW_INDEX_AUTO`). The
    /// embedded VS reads `gl_VertexIndex`, so there is no vertex buffer to bind.
    DrawAuto {
        /// Vertex count from the draw packet (3 for the fullscreen triangle).
        vertex_count: u32,
    },
    /// Bind a cached vertex buffer to a pipeline vertex-input slot (doc-4 §C4). The
    /// buffer `id` was created/uploaded through the resource cache (a prior
    /// `CreateBuffer`/`UploadBuffer` or `ImportBuffer` in this list); `stride` is the V#
    /// element stride the pipeline's vertex-input state was built against. The display
    /// thread records `vkCmdBindVertexBuffers(slot, [id's buffer])`.
    BindVertexBuffer {
        /// Vertex-input binding slot the buffer is bound at.
        slot: u32,
        /// Guest-minted [`ResourceId`] of the cached vertex buffer.
        id: ResourceId,
        /// Per-vertex stride in bytes (from the V#).
        stride: u32,
    },
    /// Bind a cached buffer as an SSBO at `(set, binding)` and supply the recompiled VS's
    /// fetch clamp (`num_records`) as a push constant (doc-4 §C4). Emitted for a
    /// recompiled VS that fetches vertices via a `StorageBuffer` indexed by
    /// `gl_VertexIndex` (it declares no vertex-input), in place of `BindVertexBuffer`. The
    /// buffer `id` was created/uploaded through the resource cache (a prior
    /// `CreateBuffer`/`UploadBuffer` in this list); the display thread allocates a
    /// descriptor set against the bound pipeline's set layout, points it at the buffer,
    /// binds it, and pushes `num_records` before the draw. A zero `num_records` clamps
    /// every vertex fetch to element 0, so the V#'s real count must be supplied here.
    BindStorageBuffer {
        /// Descriptor-set index the SSBO is bound at (matches the pipeline's set layout).
        set: u32,
        /// Binding index within the set.
        binding: u32,
        /// Guest-minted [`ResourceId`] of the cached vertex data buffer.
        id: ResourceId,
        /// The V#'s element count, pushed as the VS fetch clamp.
        num_records: u32,
    },
    /// An indexed draw (`IT_DRAW_INDEX_2`). The index buffer `id` was pulled through the
    /// resource cache; `index_type` is the element width from `IT_INDEX_TYPE`. The
    /// display thread binds the index buffer and records `vkCmdDrawIndexed`.
    DrawIndexed {
        /// Guest-minted [`ResourceId`] of the cached index buffer.
        id: ResourceId,
        /// Number of indices to draw.
        index_count: u32,
        /// Index element width (16- or 32-bit).
        index_type: IndexType,
    },
    /// Set the dynamic viewport for the following draws (`vkCmdSetViewport`, doc-4 §5).
    /// The pipeline declares `VK_DYNAMIC_STATE_VIEWPORT`, so the register-derived rect
    /// crosses the channel as plain data rather than being baked into the pipeline.
    SetViewport(ViewportRect),
    /// Set the dynamic scissor for the following draws (`vkCmdSetScissor`, doc-4 §5).
    SetScissor(ScissorRect),
    /// Create host VRAM for a cached resource under the guest-minted `id` (doc-4 §8).
    /// The display thread records `id -> vk::Buffer`; the channel version of
    /// [`GpuBackend::create_resource`], carrying the id because a fire-and-forget send
    /// cannot round-trip a backend-minted one back to the guest-thread cache.
    CreateBuffer {
        /// Guest-minted [`ResourceId`] (see [`GpuBackend::create_resource`]).
        id: ResourceId,
        /// Byte size of the resource to allocate.
        size: u64,
    },
    /// Upload a byte snapshot into cached resource `id` at `offset` (doc-4 §8). The
    /// channel version of [`GpuBackend::upload`]; the bytes are a shared snapshot the
    /// guest-thread cache took of the current guest range, owned by the command so the
    /// display thread can replay it without re-reading guest memory.
    UploadBuffer {
        /// Guest-minted [`ResourceId`] the bytes are uploaded into.
        id: ResourceId,
        /// Byte offset within the resource.
        offset: u64,
        /// The uploaded bytes (snapshot of the guest range at emit time).
        data: std::sync::Arc<[u8]>,
    },
    /// Zero-copy import of an identity-mapped guest range under the guest-minted `id`
    /// (doc-4 §8.2). The channel version of [`GpuBackend::try_import_host_range`], but
    /// the import decision is made **guest-side** by the cache's [`ImportProbe`]:
    /// emitting this command asserts the range must be imported. The display thread
    /// resolves the host pointer for `[addr, addr+size)` from the identity mapping and
    /// imports; a display-side import that the guest-side probe promised but the device
    /// cannot honor **panics** — it is a fatal invariant violation, never a silent copy
    /// fallback (a silent fallback would desync the cache's dirty assumptions — imported
    /// entries are never re-uploaded).
    ///
    /// [`ImportProbe`]: crate::gpu
    ImportBuffer {
        /// Guest-minted [`ResourceId`] the imported range is bound to.
        id: ResourceId,
        /// Guest address of the range to import (identity-mapped, whole-run stable).
        addr: u64,
        /// Byte size of the range.
        size: u64,
    },
    /// Destroy the cached resource `id` and free (or revoke, for a zero-copy import) its
    /// backend allocation (doc-4 §8). Emitted when the guest frees/unmaps the backing
    /// guest range (`sceKernelReleaseDirectMemory`/`munmap`): the guest-side cache drops
    /// the entry that keyed on that range and appends this so the display thread tears the
    /// vk resource down. Fence-safe on the display side — it waits on the in-flight draw
    /// list's fence before freeing, so a resource the GPU may still read THIS frame is not
    /// pulled out from under it. Covers both copy buffers (`CreateBuffer`) and imports
    /// (`ImportBuffer`): the backend knows which map `id` lives in, so one variant frees
    /// either without the guest side having to say which. An unknown `id` is a no-op.
    FreeResource {
        /// Guest-minted [`ResourceId`] of the resource to destroy/revoke.
        id: ResourceId,
    },
    /// Create a sampled image under the guest-minted `id` (doc-4 §C3/§C4). The display
    /// thread allocates a `vk::Image` + view + device memory of `width`×`height` in
    /// `format` (portability subset: `R8G8B8A8_UNORM` only) and records `id -> image`.
    /// The pixels arrive separately via [`UploadImage`](Self::UploadImage); this only
    /// reserves the resource so the id can be bound before the upload lands. The channel
    /// version of the create half of the sampled-texture path — the executor names no
    /// `vk::*` type. An id already present is a no-op (defensive; the cache emits one
    /// create per image).
    CreateImage {
        /// Guest-minted [`ResourceId`] the image is recorded under.
        id: ResourceId,
        /// Texel width.
        width: u32,
        /// Texel height.
        height: u32,
        /// Host pixel format (portability subset: `R8G8B8A8Unorm`).
        format: TextureFormat,
    },
    /// Upload a **detiled linear RGBA** pixel snapshot into the sampled image `id`
    /// (doc-4 §C3). The bytes are the tile-detiled linear texels (the `detile(...) ->
    /// linear` step in `ps4-gnm::cache::tile`), owned by the command as an `Arc<[u8]>` so
    /// the display thread can stage them into the image without re-reading guest memory —
    /// the same reference-counted-payload reason [`UploadBuffer`](Self::UploadBuffer) is
    /// non-`Copy`. The display thread copies them through a staging buffer and transitions
    /// the image to `SHADER_READ_ONLY_OPTIMAL`. An unknown `id` (never created) is a no-op.
    UploadImage {
        /// Guest-minted [`ResourceId`] the pixels are uploaded into.
        id: ResourceId,
        /// The detiled linear RGBA pixel bytes (`width * height * 4`).
        data: std::sync::Arc<[u8]>,
    },
    /// Create a sampler under the guest-minted `id` (doc-4 §C4). The display thread builds
    /// a `vk::Sampler` from `desc` (fixed portable defaults this phase — linear filter,
    /// repeat, no anisotropy/mips) and records `id -> sampler`. The channel version of the
    /// sampler half of the sampled-texture path. An id already present is a no-op.
    CreateSampler {
        /// Guest-minted [`ResourceId`] the sampler is recorded under.
        id: ResourceId,
        /// The sampler parameters (fixed defaults now; S#-derived later).
        desc: SamplerDesc,
    },
    /// Bind a sampled image + sampler as a combined image-sampler at `(set, binding)` for
    /// the following draw (doc-4 §C4). Emitted for a pipeline whose
    /// [`CreatePipeline::texture`](Self::CreatePipeline) declared a matching binding: the
    /// display thread allocates a descriptor set against the bound pipeline's set layout,
    /// writes the image `image_id`'s view + the sampler `sampler_id` into `(set, binding)`,
    /// and binds it before the draw. Both ids were created by a prior `CreateImage`/
    /// `UploadImage` and `CreateSampler` in this list. An unknown id defers the write.
    BindTexture {
        /// Descriptor-set index the combined image-sampler is bound at.
        set: u32,
        /// Binding index within the set.
        binding: u32,
        /// Guest-minted [`ResourceId`] of the sampled image (its view is sampled).
        image_id: ResourceId,
        /// Guest-minted [`ResourceId`] of the sampler.
        sampler_id: ResourceId,
    },
}

/// The present/sync surface the PM4 executor (`ps4-gnm`, phase 3) drives when a
/// `SubmitAndFlip` command crosses the command stream (doc-4 §3 thread boundary).
///
/// The Vulkan device lives on the display thread; the executor runs on the guest
/// thread inside the `libSceGnmDriver` submit handler and must never touch Vulkan.
/// So — unlike [`GpuBackend`], which is the display thread's own handle to the
/// device — this trait is the *guest-thread* seam: its sole impl (over the
/// `GpuManager` channel in `ps4-gpu`) ships the flip across the existing crossbeam
/// channel and blocks on the block-until-vsync handshake `videoout` already uses
/// (doc-4 §3: "SubmitAndFlip reuses the current block-until-vsync handshake").
/// Keeping it here, next to `GpuBackend`, keeps `ps4-gnm` Vulkan-free: it names
/// only this trait, never `GpuManager`/`ash::vk`.
///
/// GPU→CPU sync (EOP/EOS labels, doc-4 §C2) is *not* a method here: for phase 3 it
/// is a synchronous write into identity-mapped guest memory the executor performs
/// itself, so it needs no thread crossing.
pub trait PresentSink: Send + Sync {
    /// Present the frame the current `SubmitAndFlip` names. Reuses the softgpu
    /// present path end-to-end: the impl sends a flip over the display
    /// channel and blocks until the display thread has presented it.
    fn submit_and_flip(&self);

    /// Ship a recorded [`BackendCmd`] list to the display thread to replay against
    /// the backend (doc-4 §3: the guest-thread executor emits a data list; the
    /// display thread owns the device). This is the phase-3.5 embedded-draw seam —
    /// the executor names only this Vulkan-free trait, never `GpuManager`/`ash::vk`.
    ///
    /// Defaulted to a no-op so a present-only sink (and the phase-3 tests) need not
    /// implement it; the real `ps4-gpu` impl sends a `RunCommandList` over the
    /// channel. Headless (no sink wired) skips draws entirely, like `submit_and_flip`.
    fn run_command_list(&self, cmds: &[BackendCmd]) {
        let _ = cmds;
    }
}

static PRESENT_SINK: crate::registered::Registered<dyn PresentSink> =
    crate::registered::Registered::new();

/// Register the process-global present sink, mirroring [`crate::kernel::register_kernel`].
/// The app wires the `ps4-gpu` impl (over `GpuManager`) at boot; the `ps4-gnm`
/// executor reaches it through [`present_sink`] at submit time. Called once at boot,
/// before guest threads start, so the write lock is uncontended and can't be poisoned;
/// a failed lock is silently ignored rather than logged.
pub fn register_present_sink(sink: std::sync::Arc<dyn PresentSink>) {
    PRESENT_SINK.register(sink);
}

/// The registered present sink, or `None` when none is wired (headless: no display
/// thread, so `SubmitAndFlip` is decoded/traced but not presented).
pub fn present_sink() -> Option<std::sync::Arc<dyn PresentSink>> {
    PRESENT_SINK.get()
}

/// Geometry of a registered videoout display buffer (`sceVideoOutRegisterBuffers`):
/// the guest framebuffer a `CB_COLOR0_BASE` may alias. Plain data — the width/height
/// the display already knows, exposed so the `ps4-gnm` draw path can map an RT base
/// to the framebuffer without depending on the kernel/videoout crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayBuffer {
    /// Guest base address of the framebuffer (matches `CB_COLOR0_BASE << 8`).
    pub base: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Lookup seam from a color-target base address to the registered display buffer it
/// aliases (doc-4 §5 "map RT to the videoout fb when the base matches a registered
/// display buffer"). Wired at boot by whoever owns the display-buffer registration
/// (the videoout/kernel side); consulted by the `ps4-gnm` draw path, which stays
/// Vulkan-free by naming only this trait. Registered like [`PresentSink`] /
/// [`crate::bounded_read::BoundedRead`] so gnm reaches it without a reverse dependency.
pub trait DisplayBufferSource: Send + Sync {
    /// The display buffer whose base equals `base`, or `None` if `base` names no
    /// registered framebuffer (an arbitrary RT — out of scope this phase, the draw
    /// defers).
    fn lookup(&self, base: u64) -> Option<DisplayBuffer>;
}

static DISPLAY_BUFFERS: crate::registered::Registered<dyn DisplayBufferSource> =
    crate::registered::Registered::new();

/// Register the process-global display-buffer source, mirroring [`register_present_sink`].
/// Called once at boot before guest threads start (uncontended write lock).
pub fn register_display_buffers(source: std::sync::Arc<dyn DisplayBufferSource>) {
    DISPLAY_BUFFERS.register(source);
}

/// The registered display-buffer source, or `None` when none is wired (headless: the
/// draw path then treats every RT base as an unregistered/arbitrary RT and defers).
pub fn display_buffers() -> Option<std::sync::Arc<dyn DisplayBufferSource>> {
    DISPLAY_BUFFERS.get()
}

/// **Test-only**: the process-global display-buffer [`crate::registered::Registered`],
/// so tests can take a panic-safe RAII override (the wired vs headless RT-mapping
/// paths) without leaking a wired source into an unrelated test in the same process.
#[cfg(any(test, feature = "test-hooks"))]
pub fn registered_display_buffers()
-> &'static crate::registered::Registered<dyn DisplayBufferSource> {
    &DISPLAY_BUFFERS
}

/// The guest free/unmap → resource-cache invalidation seam (doc-4 §8). The kernel memory
/// manager calls [`Self::notify_free`] when the guest releases a direct-memory range or
/// `munmap`s a mapping; the `ps4-gnm` impl drops every cache entry keyed on that range
/// (so a free+realloc of the same address mints a fresh id + re-creates instead of a
/// stale-id clean hit), unwatches it for dirty tracking, and — for a zero-copy import —
/// revokes the backend's external-memory buffer so it does not dangle into the freed host
/// pages the GPU would otherwise keep reading.
///
/// The trait lives here, not in `ps4-gnm`, so `ps4-kernel` can fire it without depending
/// on `ps4-gnm`: the impl registers itself at boot through the global below, exactly like
/// [`register_present_sink`] / [`crate::dirty::register_dirty_source`]. The signal crosses
/// as plain `(addr, size)` data; the impl turns it into the vk teardown via a
/// [`BackendCmd::FreeResource`] over the existing display channel, keeping `ps4-gnm`
/// Vulkan-free.
pub trait MemoryFreeSink: Send + Sync {
    /// The guest freed/unmapped `[addr, addr + size)`. Evict every cache entry whose
    /// backing range overlaps it and revoke any import over it. Idempotent: a range with
    /// no cached entry is a no-op.
    fn notify_free(&self, addr: u64, size: u64);
}

static MEMORY_FREE_SINK: crate::registered::Registered<dyn MemoryFreeSink> =
    crate::registered::Registered::new();

/// Register the process-global memory-free sink, mirroring [`register_present_sink`].
/// The app wires the `ps4-gnm` impl at boot; the kernel memory manager reaches it through
/// [`memory_free_sink`] on `munmap`/`sceKernelReleaseDirectMemory`. Called once at boot,
/// before guest threads start (uncontended write lock).
pub fn register_memory_free_sink(sink: std::sync::Arc<dyn MemoryFreeSink>) {
    MEMORY_FREE_SINK.register(sink);
}

/// The registered memory-free sink, or `None` when none is wired (headless / no GPU: a
/// guest free then simply doesn't reach a resource cache, which is correct — there is no
/// cache to invalidate).
pub fn memory_free_sink() -> Option<std::sync::Arc<dyn MemoryFreeSink>> {
    MEMORY_FREE_SINK.get()
}

#[cfg(test)]
mod texture_cmd_tests {
    //! Headless serialization/shape units for the sampled-texture BackendCmd variants
    //! (doc-4 §C3/§C4, AC #1). Assert each command's fields against hand-reasoned expected
    //! values (not values read back from a production builder), and that the Arc<[u8]>
    //! pixel payload survives the Vec<BackendCmd> clone the display channel performs.

    use super::*;
    use std::sync::Arc;

    #[test]
    fn create_image_and_sampler_carry_their_fields() {
        // A hand-built create-image command names an id + extent + the one portable format.
        let cmd = BackendCmd::CreateImage {
            id: ResourceId(7),
            width: 64,
            height: 32,
            format: TextureFormat::R8G8B8A8Unorm,
        };
        match cmd {
            BackendCmd::CreateImage {
                id,
                width,
                height,
                format,
            } => {
                assert_eq!(id, ResourceId(7));
                assert_eq!(width, 64);
                assert_eq!(height, 32);
                assert_eq!(format, TextureFormat::R8G8B8A8Unorm);
            }
            other => panic!("expected CreateImage, got {other:?}"),
        }

        // A sampler command carries the fixed portable defaults this phase supplies.
        let desc = SamplerDesc {
            mag_filter: SamplerFilter::Linear,
            min_filter: SamplerFilter::Linear,
            address_mode: SamplerAddressMode::Repeat,
        };
        let cmd = BackendCmd::CreateSampler {
            id: ResourceId(9),
            desc,
        };
        match cmd {
            BackendCmd::CreateSampler { id, desc } => {
                assert_eq!(id, ResourceId(9));
                assert_eq!(desc.mag_filter, SamplerFilter::Linear);
                assert_eq!(desc.min_filter, SamplerFilter::Linear);
                assert_eq!(desc.address_mode, SamplerAddressMode::Repeat);
            }
            other => panic!("expected CreateSampler, got {other:?}"),
        }
    }

    #[test]
    fn bind_texture_names_image_and_sampler_at_binding() {
        // A bind names both resource ids at a (set, binding) — hand-reasoned expected shape.
        let cmd = BackendCmd::BindTexture {
            set: 0,
            binding: 1,
            image_id: ResourceId(7),
            sampler_id: ResourceId(9),
        };
        match cmd {
            BackendCmd::BindTexture {
                set,
                binding,
                image_id,
                sampler_id,
            } => {
                assert_eq!(set, 0);
                assert_eq!(binding, 1);
                assert_eq!(image_id, ResourceId(7));
                assert_eq!(sampler_id, ResourceId(9));
            }
            other => panic!("expected BindTexture, got {other:?}"),
        }
    }

    #[test]
    fn upload_image_pixels_survive_the_channel_clone() {
        // The Arc<[u8]> pixel snapshot must round-trip a Vec<BackendCmd> clone (the display
        // channel ships a cloned list) byte-for-byte — the whole point of the owned payload.
        let pixels: Arc<[u8]> = Arc::from(vec![1u8, 2, 3, 4, 5, 6, 7, 8]);
        let list = vec![BackendCmd::UploadImage {
            id: ResourceId(7),
            data: pixels.clone(),
        }];
        let shipped = list.clone();
        match &shipped[0] {
            BackendCmd::UploadImage { id, data } => {
                assert_eq!(*id, ResourceId(7));
                assert_eq!(&data[..], &[1, 2, 3, 4, 5, 6, 7, 8]);
            }
            other => panic!("expected UploadImage, got {other:?}"),
        }
    }

    #[test]
    fn create_pipeline_declares_its_texture_binding() {
        // A textured pipeline carries a Some(TextureBinding) so the backend adds the
        // combined image-sampler descriptor; the vertex-pull/embedded paths carry None.
        let cmd = BackendCmd::CreatePipeline {
            id: PipelineId(1),
            vs_spirv: Arc::from(vec![0u32]),
            ps_spirv: Arc::from(vec![0u32]),
            key: Box::new(PipelineKey::default()),
            target: TargetDesc::default(),
            storage: None,
            push_constants: None,
            texture: Some(TextureBinding { set: 0, binding: 1 }),
        };
        match cmd {
            BackendCmd::CreatePipeline { texture, .. } => {
                assert_eq!(texture, Some(TextureBinding { set: 0, binding: 1 }));
            }
            other => panic!("expected CreatePipeline, got {other:?}"),
        }
    }
}
