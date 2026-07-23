//! GPU backend seam (doc-2 §2).
//!
//! A narrow trait capturing *what* the command processor asks the GPU to do,
//! never *how* ash/Vulkan does it. Kept Vulkan-free so `ps4-gnm` (the future PM4
//! command processor) can target it without ever naming an `ash::vk` type, and so
//! a native Metal backend can later be an alternative impl rather than a rewrite.
//!
//! Only the present + zero-copy-import surface is implemented today (phase 1).
//! `create_target`/`create_resource`/`upload` are stubs; draw/bind/pipeline/sync
//! methods are deliberately absent and grow one-per-phase (doc-2 §2 DEFER, §(b)).

/// Opaque, backend-owned handle for a color/render target (including the videoout
/// framebuffer). The command processor holds these; it never holds `vk::*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TargetId(pub u32);

/// Opaque, backend-owned handle for a cached buffer/texture (doc-2 §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceId(pub u32);

/// Opaque handle naming a host graphics pipeline across the display-thread channel
/// (doc-2 §4). Like [`ResourceId`], it is minted **guest-side** from a monotonic
/// counter — a fire-and-forget `BackendCmd` cannot round-trip a backend-minted id back
/// (doc-2 §3). The guest-side pipeline cache assigns one id per distinct
/// [`PipelineKey`]; the display thread records `id -> vk::Pipeline` in its own map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PipelineId(pub u32);

/// Host-facing color format of a render target (doc-2 §C3). Vulkan-free: an
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

/// The tiling/swizzle layout carried on a render target (doc-2 §C3/§C9). The first
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

/// Whether a color target aliases the videoout framebuffer or is an off-guest-range
/// offscreen render target (doc-2 §5/§8.5, task-56). Carried on [`TargetDesc`] so the
/// executor can tell a draw that renders into the display buffer (present path unchanged)
/// from one that renders into an offscreen RT (which must be created + registered so a
/// later draw can sample it host-side, RT-as-texture).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TargetKind {
    /// The target aliases a registered videoout display buffer (the present path). The
    /// default: the phase-3.5/4 corpus renders the fullscreen quad here.
    #[default]
    Videoout,
    /// An offscreen render target keyed on its guest `[base, base+size)` range (task-56).
    /// A draw into it emits a `CreateRenderTarget` and registers the range so a later draw
    /// that samples the same range binds the RT host-side instead of detiling guest bytes.
    Offscreen {
        /// Guest base address of the target (`CB_COLOR0_BASE << 8`).
        base: u64,
        /// Byte size of the target's guest range (`pitch * height * bytes_per_pixel`).
        size: u64,
    },
}

/// Description of a render target to create (doc-2 §5/§C3). Derived at draw time from
/// the shadow `CB_COLOR0_*` registers and, for a target that aliases the videoout
/// framebuffer, the registered display-buffer geometry. Plain data; the backend maps
/// `format` to a `vk::Format`. Grows per phase (MRT>1, depth) as the corpus needs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetDesc {
    /// CONTENT width in pixels — the extent a consumer samples at UV `[0,1]`, and the width
    /// of the host image. NOT the row stride: an offscreen target's `pitch` is
    /// alignment-padded and may exceed this (960 content in a 1024 pitch, task-180).
    pub width: u32,
    /// CONTENT height in pixels, the row counterpart of [`Self::width`]. An offscreen
    /// target's guest allocation may hold more rows than this (540 content in 576 allocated
    /// rows, task-180); the padded row count lives only in
    /// [`TargetKind::Offscreen::size`](TargetKind::Offscreen), which is what the aliasing key
    /// is computed from.
    pub height: u32,
    /// Row pitch in pixels (from `CB_COLOR0_PITCH`), or `width` when it aliases the
    /// videoout framebuffer and no separate pitch was programmed. Padded to the surface's
    /// alignment, so `pitch >= width`; it is a *stride*, never a sampled extent.
    pub pitch: u32,
    /// Host color format (from `CB_COLOR0_INFO`).
    pub format: ColorFormat,
    /// Tiling/compression layout (from `CB_COLOR0_ATTRIB`/`INFO`, §C3/§C9).
    pub tiling: Tiling,
    /// Whether this aliases the videoout framebuffer or is an offscreen RT (task-56). The
    /// default [`TargetKind::Videoout`] keeps every existing target (present path) unchanged.
    pub kind: TargetKind,
}

/// Description of a cached resource to create. Grows per phase; a placeholder today.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceDesc {
    pub size: u64,
}

/// A storage-buffer (SSBO) binding a recompiled vertex shader fetches its vertices
/// through (doc-2 §C4). A GCN passthrough VS reads no vertex-input: it fetches each
/// vertex from an SSBO at `(set, binding)` indexed by `gl_VertexIndex`, so the host
/// pipeline needs a descriptor-set layout with one `STORAGE_BUFFER` binding rather than
/// vertex-input attributes. `stride` is the recompiler's DEFAULT per-element byte stride
/// (16 = one `vec4`) reported for reference: the true per-draw stride is NOT baked and NOT
/// specialized at create — the recompiled VS reads it from a PUSH CONSTANT the draw path
/// pushes via `BindStorageBuffer` (task-140), so one pipeline serves any stride and stride
/// stays out of the pipeline key. Absent from `CreatePipeline::vertex_storage` (an empty
/// `Vec`) for an embedded shader, which fetches nothing and uses the empty-layout
/// `gl_VertexIndex` path; multi-stream vertex fetch (task-153) carries one per V# stream.
/// Vulkan-free plain data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StorageBinding {
    /// Descriptor-set index the SSBO is bound at.
    pub set: u32,
    /// Binding index within the set.
    pub binding: u32,
    /// The recompiler's DEFAULT per-element byte stride (16 = one `vec4`), for reference.
    /// Not baked into the SPIR-V and not the live value: the draw pushes the guest V#'s
    /// real stride as a push constant via `BindStorageBuffer` (task-140).
    pub stride: u32,
}

/// The push-constant range a recompiled vertex shader reads its fetch parameters from
/// (doc-2 §C4). A GCN passthrough VS clamps every fetch index to `num_records` and reads
/// the stride/dst_sel from this block; the host pipeline must declare a matching
/// push-constant range and the draw path must supply the V#'s values at draw time (a
/// zero/missing `num_records` silently clamps every vertex to element 0). With multi-stream
/// vertex fetch (task-153) the block spans ALL stream groups: it is derived from
/// `max(offset+size)` across every `PushConstantField`, so for N streams (each a 4-uint
/// group of {num_records, stride, dst_sel, format} at byte offset `16*stream`) it naturally
/// sizes to `16*N`. The single-stream case is `offset=0, size=16`. Vulkan-free plain data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PushConstantRange {
    /// Byte offset of the range within the push-constant block (0 — the range starts at the
    /// first stream group).
    pub offset: u32,
    /// Range size in bytes — the span of all stream groups (`12*N` for N streams).
    pub size: u32,
}

/// Host-facing pixel format of a sampled texture (doc-2 §C3/§C4). Kept deliberately
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
    /// 8-8-8-8 sRGB, RGBA channel order — same texel bytes as [`Self::R8G8B8A8Unorm`], but
    /// the T# number format is sRGB (`IMG_NUM_FORMAT_SRGB`). The backend maps this to an
    /// `_SRGB` `vk::Format` so `OpImageSample` auto-decodes the texel to LINEAR, letting the
    /// fragment shader composite in linear space (task-154 residual #2).
    R8G8B8A8Srgb,
}

/// The minification/magnification filter a sampler applies (doc-2 §C4). Vulkan-free
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

/// The out-of-`[0,1]` address behaviour a sampler applies per axis (doc-2 §C4).
/// Vulkan-free identity the backend maps to `vk::SamplerAddressMode`. Decoded per axis
/// from the guest S#'s `CLAMP_X`/`CLAMP_Y` fields (see [`crate::gpu`] consumers /
/// `ps4_gnm::vbuf::decode_s_sharp`), so a wrapping texture wraps and a clamped one clamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SamplerAddressMode {
    /// Wrap (`VK_SAMPLER_ADDRESS_MODE_REPEAT`).
    #[default]
    Repeat,
    /// Mirrored wrap (`VK_SAMPLER_ADDRESS_MODE_MIRRORED_REPEAT`).
    MirrorRepeat,
    /// Clamp to the edge texel (`VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE`).
    ClampToEdge,
}

/// Description of a sampler to create (doc-2 §C4). Plain data derived from an S# (filter
/// bit + per-axis `CLAMP_X`/`CLAMP_Y` wrap fields); no anisotropy/mips (decision-3). The
/// backend maps it to a `vk::Sampler`. Vulkan-free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SamplerDesc {
    /// Magnification filter.
    pub mag_filter: SamplerFilter,
    /// Minification filter.
    pub min_filter: SamplerFilter,
    /// U-axis (`CLAMP_X`) address mode.
    pub address_mode_u: SamplerAddressMode,
    /// V-axis (`CLAMP_Y`) address mode.
    pub address_mode_v: SamplerAddressMode,
}

/// One combined image-sampler binding a pipeline declares so a pixel shader can sample a
/// texture (doc-2 §C4). Carried on [`BackendCmd::CreatePipeline`] alongside
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
/// to get-or-create a host pipeline (doc-2 §4/§5). It carries a shader **identity**
/// (a 64-bit hash per stage, from the `ShaderRef`), the vertex-input layout, the RT
/// format and the blend/depth bits — **not** a hardcoded pipeline handle (doc-2 §4:
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
    /// The bound-resource layout the pipeline is built against (task-130 slice 6). Two
    /// draws with the SAME shader hashes but DIFFERENT descriptor layouts (a storage/
    /// const/texture binding at a different `(set, binding)`) build DIFFERENT host
    /// pipelines, so the cache must not reuse one for the other. Keyed here so a
    /// differing layout re-keys the pipeline (anti-silent-wrong-reuse).
    pub resources: ResourceSignature,
    /// Color-target format the pipeline renders into (from `CB_COLOR0_INFO`).
    pub color_format: ColorFormat,
    /// Blend enable/equation bits (from `CB_BLEND0_CONTROL`/`CB_COLOR_CONTROL`).
    pub blend: BlendKey,
    /// Depth test/write bits (from `DB_DEPTH_CONTROL`/`DB_Z_INFO`).
    pub depth: DepthKey,
    /// Input-assembly topology the pipeline is built against, from
    /// `VGT_PRIMITIVE_TYPE` (task-184). In the key because the same shader pair and
    /// blend/depth state can be issued under different primitive types.
    pub topology: PrimitiveTopology,
}

/// The host input-assembly topology a draw rasterizes under, derived from GFX6
/// `VGT_PRIMITIVE_TYPE` (task-184). Vulkan-free, like [`ColorFormat`].
///
/// GCN's `DI_PT_RECTLIST` has no Vulkan equivalent: three vertices name three corners
/// of a parallelogram and the hardware synthesizes the fourth as `p2 + p1 - p0`. It is
/// how a PS4 title issues a full-screen fill — Celeste clears its bloom targets with
/// one. See [`PrimitiveTopology::TriangleStrip`] for how the draw path expands it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PrimitiveTopology {
    /// `DI_PT_TRILIST` (4) and every type this layer does not model — three vertices
    /// per triangle. The historical behavior, and the default.
    #[default]
    TriangleList,
    /// `DI_PT_RECTLIST` (0x11), expanded: the draw is issued with FOUR vertices under a
    /// triangle strip, whose two triangles `(v0,v1,v2)` and `(v1,v2,v3)` tile the same
    /// parallelogram the hardware rasterizes.
    ///
    /// APPROXIMATION, and the reason this is a distinct variant rather than a faithful
    /// lowering: the fourth vertex comes from the vertex stream at index 3, not from the
    /// hardware's `p2 + p1 - p0` synthesis. The two coincide for the index-derived
    /// full-screen-fill idiom (a VS computing position from `gl_VertexIndex`
    /// arithmetically, where index 3 lands exactly on the missing corner) and diverge
    /// for a rect list whose corners are fetched from a vertex buffer.
    TriangleStrip,
}

/// One descriptor's `(set, binding)` placement — the part of a resource binding that is
/// pipeline-LAYOUT-relevant (task-130 slice 6). Deliberately NOT the byte stride: the
/// vertex stride flows in as a SPIR-V PUSH CONSTANT pushed per draw (task-140), so one
/// pipeline serves every stride and stride is OUT of the key. Vulkan-free plain data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ResourceSlot {
    /// Descriptor-set index.
    pub set: u32,
    /// Binding index within the set.
    pub binding: u32,
}

/// Upper bound on the combined image-samplers ONE pixel shader may declare (task-199).
///
/// A PS declares one per distinct `image_sample` descriptor pair, so this bounds the
/// set-0 layout a single draw can ask for. The recompiler defers a shader that would
/// exceed it rather than emitting a layout the backend cannot build; [`ResourceSignature`]
/// sizes its slot array to match so the pipeline key stays `Copy`.
pub const MAX_PS_TEXTURES: usize = 8;

/// The bound-resource layout a pipeline is built against — the descriptor provenance
/// that, together with the shader hashes, uniquely names a host pipeline (task-130 slice
/// 6). The slots mirror the descriptor kinds a recompiled shader can declare: the
/// vertex-fetch SSBO (VS), the scalar constant buffer (VS), and the combined
/// image-samplers (PS). `None` = the shader declares no descriptor of that kind.
///
/// STRIDE IS OUT (task-140): the vertex element stride is a PUSH CONSTANT pushed per draw,
/// so a single pipeline serves every stride — the stride flows in at bind time, not by
/// re-keying or re-specialization. Only the set/binding provenance (which re-keys iff the
/// *layout* differs) is carried here. Vulkan-free plain data; `Hash`/`Eq` so it folds into
/// [`PipelineKey`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ResourceSignature {
    /// The vertex-fetch storage buffer (VS SSBO) placement, or `None`.
    pub storage: Option<ResourceSlot>,
    /// The VERTEX-stage scalar constant-buffer (`s_buffer_load`) placement, or `None`.
    pub const_storage: Option<ResourceSlot>,
    /// The FRAGMENT-stage scalar constant-buffer placement, or `None` (task-174). Distinct
    /// from `const_storage` so a VS+PS dual-CB layout keys to a distinct pipeline.
    pub const_storage_fragment: Option<ResourceSlot>,
    /// The combined image-sampler (PS) placements, in the shader's first-sample order —
    /// index `i` is the `i`-th texture the PS declares, `None` past the last one
    /// (task-199). A fixed array rather than a `Vec` so [`PipelineKey`] stays `Copy` and
    /// hashable; a PS that samples nothing leaves every entry `None`.
    pub textures: [Option<ResourceSlot>; MAX_PS_TEXTURES],
}

impl ResourceSignature {
    /// The declared texture slots, in order, without the trailing `None` padding.
    pub fn textures(&self) -> impl Iterator<Item = ResourceSlot> + '_ {
        self.textures.iter().flatten().copied()
    }

    /// How many combined image-samplers the pipeline declares.
    pub fn texture_count(&self) -> usize {
        self.textures.iter().flatten().count()
    }
}

/// The host vertex-attribute format a derived attribute carries (doc-2 §C4). A
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

/// One vertex attribute the pipeline declares (doc-2 §C4): its shader `location`, the
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

/// One vertex-buffer binding the pipeline declares (doc-2 §C4): the `binding` slot and
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

/// Vertex-input layout a pipeline is built against (doc-2 §4/§C4). The register-derived
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

/// The blend bits a draw snapshots into its [`PipelineKey`] (doc-2 §5). Modeled as the
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
    /// MRT0's per-component colour write mask — `CB_TARGET_MASK.TARGET0_ENABLE`, bits
    /// `[3:0]` = R,G,B,A (the same component order Vulkan's `ColorComponentFlags` uses).
    ///
    /// Not cosmetic: a guest that masks ALPHA off while rendering a premultiplied-alpha
    /// intermediate (Celeste's bloom chain) relies on the target keeping the alpha its
    /// clear left there. Forcing an RGBA write instead stores alpha = 1, and the later
    /// `ONE / ONE_MINUS_SRC_ALPHA` composite of that target then degenerates from "add the
    /// glow" to "replace the framebuffer" — a full-screen wipe.
    pub write_mask: u8,
}

/// A [`BlendKey::control`] word split into its GFX6 `CB_BLENDn_CONTROL` fields.
///
/// Vulkan-free, so both the backend (which maps each field to a `vk::BlendFactor`/`vk::BlendOp`)
/// and the GPU-state snapshot (which prints their names, task-185 round 2) share ONE copy of
/// the bit layout. The field positions are the thing that drifts — a snapshot decoding
/// `0x45010501` with its own private shifts could disagree with the pipeline the frame
/// actually ran, which is exactly the class of lie the snapshot exists to prevent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlendFields {
    /// `COLOR_SRCBLEND` `[4:0]`.
    pub color_src: u32,
    /// `COLOR_COMB_FCN` `[7:5]`.
    pub color_comb: u32,
    /// `COLOR_DESTBLEND` `[12:8]`.
    pub color_dst: u32,
    /// `ALPHA_SRCBLEND` `[20:16]`, or the colour source when `separate_alpha` is clear.
    pub alpha_src: u32,
    /// `ALPHA_COMB_FCN` `[23:21]`, or the colour equation when `separate_alpha` is clear.
    pub alpha_comb: u32,
    /// `ALPHA_DESTBLEND` `[28:24]`, or the colour destination when `separate_alpha` is clear.
    pub alpha_dst: u32,
    /// `SEPARATE_ALPHA_BLEND` (bit 29). When clear, alpha MIRRORS colour and the three
    /// `alpha_*` fields above are copies rather than the raw register bits.
    pub separate_alpha: bool,
}

impl BlendKey {
    /// Split [`Self::control`] into its GFX6 fields, resolving the `SEPARATE_ALPHA_BLEND`
    /// mirroring so a caller never has to re-implement that rule.
    pub fn fields(&self) -> BlendFields {
        let c = self.control;
        let color_src = c & 0x1F;
        let color_comb = (c >> 5) & 0x7;
        let color_dst = (c >> 8) & 0x1F;
        let separate_alpha = (c >> 29) & 0x1 != 0;
        let (alpha_src, alpha_comb, alpha_dst) = if separate_alpha {
            ((c >> 16) & 0x1F, (c >> 21) & 0x7, (c >> 24) & 0x1F)
        } else {
            (color_src, color_comb, color_dst)
        };
        BlendFields {
            color_src,
            color_comb,
            color_dst,
            alpha_src,
            alpha_comb,
            alpha_dst,
            separate_alpha,
        }
    }
}

/// Name a GFX6 `*_SRCBLEND`/`*_DESTBLEND` factor enum. `"?"` for a value outside the table,
/// so an unmodeled factor reads as unknown rather than as the fallback the backend picks.
pub fn blend_factor_name(factor: u32) -> &'static str {
    match factor {
        0 => "ZERO",
        1 => "ONE",
        2 => "SRC_COLOR",
        3 => "ONE_MINUS_SRC_COLOR",
        4 => "SRC_ALPHA",
        5 => "ONE_MINUS_SRC_ALPHA",
        6 => "DST_ALPHA",
        7 => "ONE_MINUS_DST_ALPHA",
        8 => "DST_COLOR",
        9 => "ONE_MINUS_DST_COLOR",
        10 => "SRC_ALPHA_SATURATE",
        _ => "?",
    }
}

/// Name a GFX6 `*_COMB_FCN` equation enum. `"?"` outside the table — see
/// [`blend_factor_name`].
pub fn blend_op_name(comb: u32) -> &'static str {
    match comb {
        0 => "ADD",
        1 => "SUBTRACT",
        2 => "MIN",
        3 => "MAX",
        4 => "REVERSE_SUBTRACT",
        _ => "?",
    }
}

/// The depth bits a draw snapshots into its [`PipelineKey`] (doc-2 §5). Depth presence
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

/// The element width of a bound index buffer (`IT_INDEX_TYPE`, doc-2 §5). GCN encodes
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

/// A screen-space viewport a draw sets dynamically (`vkCmdSetViewport`, doc-2 §5).
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

/// A screen scissor rect a draw sets dynamically (`vkCmdSetScissor`, doc-2 §5). Plain
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
/// `Send` bound the doc-2 §2 sketch shows belongs with the §3 channel-crossing
/// executor design and is added when that lands, not speculatively now (§(b)).
pub trait GpuBackend {
    // ---- presentation (phase 1: implemented, relocated from the display loop) ----

    /// Present the given host target to the display (the softgpu framebuffer today).
    fn present(&mut self, target: TargetId) -> Result<(), GpuError>;

    // ---- resource cache backing (phase 3.5+, doc-2 §8) ----

    /// Create a render target. Stub until the resource cache lands (doc-2 §8).
    fn create_target(&mut self, desc: &TargetDesc) -> TargetId;

    /// Create host VRAM for a cached resource under the caller-supplied `id`.
    ///
    /// **Id ownership (doc-2 §3 channel model):** the
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

    /// Upload host bytes into a cached resource. Stub until the cache lands (doc-2 §8).
    fn upload(&mut self, id: ResourceId, offset: u64, bytes: &[u8]);

    /// Optional zero-copy import of an identity-mapped guest range under the
    /// caller-supplied `id` (minted guest-side, see [`Self::create_resource`]).
    ///
    /// Returns `true` when the range was imported zero-copy (`id` now names the
    /// imported buffer); `false` when the backend/range can't import (MoltenVK,
    /// unaligned) so the caller falls back to `create_resource` + `upload` — making
    /// zero-copy vs copy a single seam (doc-2 §8.2).
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
/// display thread to replay against the real backend (doc-2 §3: the executor runs
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
    /// guest-minted `id` (doc-2 §4, decision-7). Emitted **once per distinct pipeline**
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
        /// The vertex-pull SSBO bindings a recompiled VS fetches vertices through, one per
        /// distinct V# stream (multi-stream vertex fetch, task-153) — up to 4 (e.g. set 0,
        /// bindings 0/3/4/5). **Empty** for an embedded shader (it fetches nothing and uses
        /// the `gl_VertexIndex` path). When non-empty, the backend builds a descriptor-set
        /// layout with one `STORAGE_BUFFER` binding PER entry (VERTEX stage) and an empty
        /// vertex-input state (the VS consumes no vertex-input); when empty, the vertex-input
        /// path in `key` drives the layout. Each stream is bound at draw time by its own
        /// [`BindStorageBuffer`](Self::BindStorageBuffer) carrying that stream's
        /// num_records/stride/dst_sel and push-constant offset.
        vertex_storage: Vec<StorageBinding>,
        /// The push-constant range a recompiled VS reads its `num_records` fetch clamp
        /// from, `None` for an embedded shader. When `Some`, the backend declares a
        /// matching push-constant range on the pipeline layout.
        push_constants: Option<PushConstantRange>,
        /// The combined image-sampler bindings a pixel shader samples textures through,
        /// one per distinct `image_sample` descriptor pair in the shader's first-sample
        /// order (task-199); **empty** when the pipeline samples nothing. The backend adds
        /// one `COMBINED_IMAGE_SAMPLER` descriptor per entry at its `(set, binding)`
        /// (FRAGMENT stage) to the set-0 layout, so a later
        /// [`BindTexture`](Self::BindTexture) per entry can point each at its own image +
        /// sampler. A PS routinely mixes a register-resident T# with a memory-resident one
        /// (Celeste's distortion and colour-grade passes), and binding them to a single
        /// descriptor made every sample read the same texture.
        textures: Vec<TextureBinding>,
        /// The **VERTEX-stage** scalar constant-buffer SSBO binding a recompiled VS reads
        /// via `s_buffer_load` (the 4×4 transform matrix Celeste's VS loads, doc-6 Entry 9)
        /// — set0/bind2, `None` when the VS loads no uniform constants. When `Some`, the
        /// backend adds a `STORAGE_BUFFER` descriptor at `(set, binding)` with VERTEX
        /// `stage_flags` — distinct from the vertex-pull `storage` binding — so a later
        /// [`BindConstBuffer`](Self::BindConstBuffer) can point it at the guest constant
        /// buffer's bytes. `stride` is unused for a constant buffer (it is a flat `uint[]`),
        /// carried only to reuse [`StorageBinding`].
        const_storage: Option<StorageBinding>,
        /// The **FRAGMENT-stage** constant-buffer SSBO binding a recompiled PS reads via
        /// `s_buffer_load` (Celeste's pixel-shader constants) — set0/bind6 (task-174),
        /// distinct from the VS const at set0/bind2 so a draw whose VS AND PS both declare a
        /// constant buffer has two non-colliding set-0 slots instead of deferring. `None`
        /// when the PS loads no uniform constants. When `Some`, the backend adds a
        /// `STORAGE_BUFFER` descriptor at `(set, binding)` with FRAGMENT `stage_flags` so
        /// the layout matches the FRAGMENT SPIR-V's declared binding (else the driver
        /// faults, `VUID-VkGraphicsPipelineCreateInfo-layout-07988`); its own
        /// [`BindConstBuffer`](Self::BindConstBuffer) points it at the PS constant bytes.
        const_storage_fragment: Option<StorageBinding>,
    },
    /// Bind the host pipeline previously created under `id` (doc-2 §4, decision-7). The
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
    /// Bind a cached vertex buffer to a pipeline vertex-input slot (doc-2 §C4). The
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
    /// fetch parameters (`num_records`/`stride`/`dst_sel`) as push constants (doc-2 §C4).
    /// Emitted for a recompiled VS that fetches vertices via a `StorageBuffer` indexed by
    /// `gl_VertexIndex` (it declares no vertex-input), in place of `BindVertexBuffer`. The
    /// buffer `id` was created/uploaded through the resource cache (a prior
    /// `CreateBuffer`/`UploadBuffer` in this list); the display thread allocates a
    /// descriptor set against the bound pipeline's set layout, points `binding` at the
    /// buffer, binds it, and pushes this stream's 4-uint group at `pc_offset` before the
    /// draw. A zero `num_records` clamps every vertex fetch to element 0, so the V#'s real
    /// count must be supplied here.
    ///
    /// **Multi-stream (task-153):** one command is emitted PER V# stream, each at its own
    /// `(set, binding)` with its own num_records/stride/dst_sel/format and `pc_offset` (= `16*stream`).
    /// The executor emits N of them for an N-stream VS; the backend pushes each stream's group
    /// at its own offset into the shared push-constant range.
    BindStorageBuffer {
        /// Descriptor-set index the SSBO is bound at (matches the pipeline's set layout).
        set: u32,
        /// Binding index within the set.
        binding: u32,
        /// Guest-minted [`ResourceId`] of the cached vertex data buffer.
        id: ResourceId,
        /// The V#'s element count, pushed as the VS fetch clamp (push-constant member 0).
        num_records: u32,
        /// The V#'s per-element stride in BYTES, pushed as the VS fetch stride
        /// (push-constant member 1, offset 4). The recompiled VS reads this dynamically so
        /// one pipeline serves every stride; a non-16 stride (12/24/32…) addresses correctly
        /// without a re-emit or re-specialization, and stride stays out of the pipeline key
        /// (task-140).
        stride: u32,
        /// The V#'s packed destination swizzle (`word3[11:0]`, 4×3 bits), pushed as the VS
        /// fetch dst_sel (push-constant member 2, offset 8). The recompiled VS applies it
        /// per channel — selector `0`→`0.0`, `1`→`1.0`, `4..7`→source component — exactly
        /// as GCN's format/swizzle stage does (task-155). The identity `[4,5,6,7]` (0xFAC)
        /// is a raw passthrough; a Celeste-shaped `[4,5,6,1]` substitutes `w=1.0` so
        /// `gl_Position.w` is 1.0 instead of stride-padding garbage. Swizzle stays out of
        /// the pipeline key — one pipeline serves any swizzle.
        dst_sel: u32,
        /// The V#'s packed vertex FORMAT (`dfmt` in `[7:0]`, `nfmt` in `[15:8]`), pushed as
        /// the VS fetch format (push-constant member 3, offset 12). The recompiled VS unpacks
        /// each fetched component per this format — a 32-bit float format reads the raw dword,
        /// a packed `_8_8_8_8` UNORM (Celeste's sprite color) or `_16*` format decodes each
        /// component's byte/half and normalizes (task-164). Format stays out of the pipeline
        /// key — one pipeline serves any format. A zero value (`dfmt` 0) is the raw-dword path.
        format: u32,
        /// Byte offset into the pipeline's push-constant range where THIS stream's 4-uint
        /// group ({num_records@+0, stride@+4, dst_sel@+8, format@+12}) is written (multi-stream
        /// vertex fetch, task-153/task-164). Equals `16*stream` for 0-based stream index
        /// `stream`; `0` for the single-stream case. The recompiled VS reads its stream's group
        /// from this same offset, so the backend pushes the 16-byte group at `pc_offset` rather
        /// than always at offset 0.
        pc_offset: u32,
    },
    /// Bind a cached buffer as the **constant-buffer** SSBO at `(set, binding)` (doc-6
    /// Entry 9). Emitted for a recompiled VS that reads scalar constants via `s_buffer_load`
    /// (e.g. the 4×4 transform matrix), alongside the vertex-pull `BindStorageBuffer`. The
    /// buffer `id` was created/uploaded through the resource cache; the display thread writes
    /// it into the same set-0 descriptor set the pipeline declared its
    /// `const_storage` binding at. Unlike [`BindStorageBuffer`](Self::BindStorageBuffer) it
    /// carries no `num_records` — a constant buffer is a flat `uint[]` the shader indexes by
    /// a compile-time dword offset, not a per-vertex fetch clamp.
    BindConstBuffer {
        /// Descriptor-set index the constant-buffer SSBO is bound at.
        set: u32,
        /// Binding index within the set (the recompiler emits `2`).
        binding: u32,
        /// Guest-minted [`ResourceId`] of the cached constant-buffer bytes.
        id: ResourceId,
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
    /// Set the dynamic viewport for the following draws (`vkCmdSetViewport`, doc-2 §5).
    /// The pipeline declares `VK_DYNAMIC_STATE_VIEWPORT`, so the register-derived rect
    /// crosses the channel as plain data rather than being baked into the pipeline.
    SetViewport(ViewportRect),
    /// Set the dynamic scissor for the following draws (`vkCmdSetScissor`, doc-2 §5).
    SetScissor(ScissorRect),
    /// Create host VRAM for a cached resource under the guest-minted `id` (doc-2 §8).
    /// The display thread records `id -> vk::Buffer`; the channel version of
    /// [`GpuBackend::create_resource`], carrying the id because a fire-and-forget send
    /// cannot round-trip a backend-minted one back to the guest-thread cache.
    CreateBuffer {
        /// Guest-minted [`ResourceId`] (see [`GpuBackend::create_resource`]).
        id: ResourceId,
        /// Byte size of the resource to allocate.
        size: u64,
    },
    /// Upload a byte snapshot into cached resource `id` at `offset` (doc-2 §8). The
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
    /// (doc-2 §8.2). The channel version of [`GpuBackend::try_import_host_range`], but
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
    /// backend allocation (doc-2 §8). Emitted when the guest frees/unmaps the backing
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
    /// Create a sampled image under the guest-minted `id` (doc-2 §C3/§C4). The display
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
    /// (doc-2 §C3). The bytes are the tile-detiled linear texels (the `detile(...) ->
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
    /// Create a sampler under the guest-minted `id` (doc-2 §C4). The display thread builds
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
    /// the following draw (doc-2 §C4). Emitted for a pipeline whose
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
    /// Create an **offscreen render target** under the guest-minted `id` (doc-2 §8.5,
    /// task-56 RT-as-texture). The display thread allocates a `vk::Image` + view + memory of
    /// `width`×`height` in `format` usable as both a color attachment and a sampled source
    /// (portability subset: `R8G8B8A8_UNORM`), and records `id -> render target`. Unlike
    /// [`CreateImage`](Self::CreateImage) there is **no** paired upload: a render target is
    /// *filled by the GPU* rendering into it, never by a guest-byte snapshot — the guest
    /// bytes at its base were never written by the CPU (the whole point of RT-as-texture).
    /// A later draw that samples the same guest range binds this id via
    /// [`BindTexture`](Self::BindTexture) with zero `CreateImage`/`UploadImage`. An id
    /// already present is a no-op. The backend resource itself lands in step 3; this variant
    /// is additive and harmlessly ignored until then.
    CreateRenderTarget {
        /// Guest-minted [`ResourceId`] the render target is recorded under.
        id: ResourceId,
        /// Target width in pixels.
        width: u32,
        /// Target height in pixels.
        height: u32,
        /// Host color format (portability subset: `R8G8B8A8Unorm`).
        format: ColorFormat,
    },
    /// Read a render target's pixels back into the guest range `[addr, addr+size)`
    /// (doc-2 §8.5, task-56, opt-in via [`ReadbackPolicy`]). Emitted only when the policy is
    /// [`ReadbackPolicy::All`]; under the default [`ReadbackPolicy::Off`] no readback command
    /// is ever produced (readback is a perf cliff). The display thread copies the RT out to a
    /// host-visible staging buffer, re-tiles it to the guest surface layout, and writes the
    /// guest bytes — closing the GPU→CPU direction so a title that reads its own rendered
    /// output on the CPU sees it. The readback machinery itself is step 5; this variant is
    /// additive and harmlessly ignored until then.
    ///
    /// The guest surface's `pitch`/`tiling` travel WITH the command (task-181): the host RT
    /// image knows only its content extent, and packing the readback at the content width
    /// into a pitch-padded or tiled guest surface writes rows that no reader can decode. A
    /// tiling the re-tiler cannot express makes the backend REFUSE the write rather than
    /// leave plausible-looking bytes behind.
    ReadbackRenderTarget {
        /// Guest-minted [`ResourceId`] of the render target to read back.
        id: ResourceId,
        /// Guest address the pixels are written back to.
        addr: u64,
        /// Byte size of the guest range.
        size: u64,
        /// Guest row stride in TEXELS ([`TargetDesc::pitch`]), `>=` the RT's content width.
        /// The readback strides each row by this, never by the content width.
        pitch: u32,
        /// Guest surface tiling ([`TargetDesc::tiling`]). Decides which re-tile the readback
        /// packs with; an unsupported mode (2D macro-tiling) aborts the write.
        tiling: Tiling,
    },
    /// Write a render target's pixels to `path` as a PNG, for a GPU state snapshot
    /// (task-187). **This is a DIAGNOSTIC, and deliberately not the same thing as
    /// [`ReadbackRenderTarget`](Self::ReadbackRenderTarget).**
    ///
    /// The two are separate commands because they have separate jobs, and fusing them is
    /// what made the diagnostic useless: a readback must reproduce the GUEST's surface
    /// layout (pitch, tile mode) because guest code will read those bytes, so it refuses a
    /// 2D macro-tiled target it cannot express — and every Celeste render target is
    /// macro-tiled. To LOOK at pixels none of that matters: the host image is already
    /// linear RGBA8. So this command carries no guest address, no pitch and no tiling, and
    /// writes NOTHING to guest memory. It cannot refuse for a layout reason, because it
    /// never has a layout question to answer.
    ///
    /// Fire-and-forget: nothing travels back, so the submit thread does not wait. The
    /// snapshot recorder that requested it records the path it EXPECTED; a file that is
    /// absent means the host-side copy failed, and the display thread logged why.
    ///
    /// Emitted only while a snapshot capture is armed AND
    /// [`snapshot::render_targets_enabled`](crate::snapshot::render_targets_enabled) is on.
    /// An unknown `id`, or an RT whose producer draw did not run this submit, is skipped
    /// with a warning rather than producing a plausible-looking picture of nothing.
    DumpRenderTargetPng {
        /// Guest-minted [`ResourceId`] of the render target to copy out.
        id: ResourceId,
        /// Absolute-or-CWD-relative destination `.png`. Chosen by the submit thread (only
        /// it knows the capture's frame directory); the display thread only writes it.
        path: std::path::PathBuf,
    },
    /// Open a render pass targeting the offscreen render target `id` for the draw that
    /// follows (doc-2 §8.5, task-56 step 4, the multi-pass refactor). The executor emits
    /// this immediately before a producer draw whose target is
    /// [`TargetKind::Offscreen`](TargetKind::Offscreen): it names the RT the *next* draw
    /// renders INTO (as a color attachment), so the display thread records that draw into
    /// the RT's own render pass + framebuffer (sized to the RT extent) rather than the
    /// fixed videoout target. It applies to exactly one draw — a draw with no preceding
    /// `SetRenderTarget` renders into the videoout target as before (the present path is
    /// unchanged). Additive: a backend that predates the multi-pass refactor ignores it and
    /// the draw falls back to videoout.
    SetRenderTarget {
        /// Guest-minted [`ResourceId`] of the offscreen render target the next draw writes
        /// into. Must have been created by a prior
        /// [`CreateRenderTarget`](Self::CreateRenderTarget).
        id: ResourceId,
    },
}

/// Whether an offscreen render target's contents are read back into guest memory after a
/// draw renders into it (doc-2 §8.5, task-56). Readback is the GPU→CPU reverse direction —
/// a perf cliff (a full RT copy + re-tile + guest write per flagged RT) — so it is **off by
/// default** and gated behind the `UNEMUPS4_RT_READBACK` environment lever. A title that
/// only *samples* its render targets host-side (the common RT-as-texture case) needs no
/// readback at all; readback exists solely for the rarer case of a title that reads its own
/// rendered output back on the CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReadbackPolicy {
    /// Never emit a [`BackendCmd::ReadbackRenderTarget`]. The portable default: RT contents
    /// stay GPU-side and are only ever sampled host-side.
    #[default]
    Off,
    /// Read every flagged render target back into its guest range after the draw that fills
    /// it. Opt-in via `UNEMUPS4_RT_READBACK` (any non-empty value other than `off`/`0`).
    All,
}

impl ReadbackPolicy {
    /// Resolve the policy from the `UNEMUPS4_RT_READBACK` environment variable (doc-2 §8.5).
    /// Defaults to [`ReadbackPolicy::Off`] when unset/empty; `off`/`0`/`false` (any case)
    /// stay `Off`; any other non-empty value selects [`ReadbackPolicy::All`]. Kept a plain
    /// resolver (not cached) so tests can toggle the env and re-resolve deterministically.
    pub fn from_env() -> ReadbackPolicy {
        match std::env::var("UNEMUPS4_RT_READBACK") {
            Ok(v) => ReadbackPolicy::parse(&v),
            Err(_) => ReadbackPolicy::Off,
        }
    }

    /// Map a raw env string to a policy (the parse half of [`from_env`], factored out so it
    /// can be unit-tested without touching process env). Empty / `off` / `0` / `false`
    /// (case-insensitive, trimmed) → [`Off`]; anything else → [`All`].
    ///
    /// [`from_env`]: Self::from_env
    /// [`Off`]: Self::Off
    pub fn parse(v: &str) -> ReadbackPolicy {
        match v.trim().to_ascii_lowercase().as_str() {
            "" | "off" | "0" | "false" => ReadbackPolicy::Off,
            _ => ReadbackPolicy::All,
        }
    }
}

/// Queryable GPU capabilities, populated once at device selection (task-136).
///
/// This is the *seam* for a future caps-tiered Vulkan/MoltenVK split (roadmap
/// portability constraint, decision-3/6/7): today every populated path emits the
/// SAME portable baseline (the task-133 clamp), so nothing here gates behavior yet
/// — a caps-driven fast path is a LATER, *measured* optimization (YAGNI until a
/// clamp cost is measured). The point of landing it now is that the future fork is
/// a DATA flag threaded through one code path, never a second backend or a second
/// golden set.
///
/// Plain data on purpose: it may hold bools/limits *derived* from `ash::vk`
/// queries, but the type itself is Vulkan-free so it can live in `ps4-core` and be
/// threaded through the executor/backend without dragging `ash` into those layers.
///
/// # How it is populated (`ps4-gpu`, `VulkanContext`)
///
/// At device creation the backend queries the REAL device — never
/// `#[cfg(target_os)]`. Platform != capability: MoltenVK gains features over time
/// and Linux drivers vary, so the *only* correct signal is what the device
/// advertises. If `VK_KHR_portability_subset` is present (MoltenVK), the
/// portability restrictions below are read from
/// `VkPhysicalDevicePortabilitySubsetFeaturesKHR` /
/// `...PropertiesKHR`; if it is absent (native desktop Vulkan is a feature
/// superset), the struct reflects "full" — which is exactly [`GpuCaps::default`].
///
/// # Fields
///
/// Every `portability_*` flag is `true` when the feature IS supported. On native
/// Vulkan (extension absent) they are all `true` (nothing is restricted); on
/// MoltenVK they mirror `VkPhysicalDevicePortabilitySubsetFeaturesKHR`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuCaps {
    /// Whether the device advertises `VK_KHR_portability_subset` — i.e. this is a
    /// portability (MoltenVK/Metal) target rather than native desktop Vulkan.
    /// Informational: caps decisions read the specific flags below, NOT this bit
    /// (platform != capability).
    pub portability_subset: bool,

    // ---- portability-subset feature bits (all `true` when supported) ----
    /// `triangleFans`: `VK_PRIMITIVE_TOPOLOGY_TRIANGLE_FAN` is usable. Metal has no
    /// triangle-fan primitive, so MoltenVK reports this `false`.
    pub triangle_fans: bool,
    /// `separateStencilMaskRef`: independent front/back stencil reference values.
    pub separate_stencil_mask_ref: bool,
    /// `imageViewFormatReinterpretation`: an image view may reinterpret the texel
    /// format (bit pattern) of its image. Restricted on Metal.
    pub image_view_format_reinterpretation: bool,
    /// `imageViewFormatSwizzle`: an image view may apply a component swizzle.
    pub image_view_format_swizzle: bool,
    /// `constantAlphaColorBlendFactors`: `CONSTANT_ALPHA` blend factors are usable.
    pub constant_alpha_color_blend_factors: bool,
    /// `pointPolygons`: `VK_POLYGON_MODE_POINT` is usable.
    pub point_polygons: bool,

    // ---- relevant limits ----
    /// `VkSurfaceCapabilitiesKHR::minImageCount` for the presentation surface. A
    /// portability-aware swapchain path (the one that hit
    /// `VUID-VkSwapchainCreateInfoKHR-presentMode-02839`: min 2 vs required 3) is
    /// exactly what would consult this instead of hardcoding a count. 0 when the
    /// surface was not queried.
    pub surface_min_image_count: u32,
    /// `VkSurfaceCapabilitiesKHR::maxImageCount` (0 = no upper bound), for the same
    /// swapchain sizing concern.
    pub surface_max_image_count: u32,

    // ---- a couple of real base features that a future clamp would consult ----
    /// `VkPhysicalDeviceFeatures::samplerAnisotropy`.
    pub sampler_anisotropy: bool,
    /// `VkPhysicalDeviceFeatures::fillModeNonSolid` (wireframe/point polygon mode).
    pub fill_mode_non_solid: bool,
    /// `VkPhysicalDeviceFeatures::independentBlend` (per-attachment blend state).
    pub independent_blend: bool,
}

impl GpuCaps {
    /// The "full" / no-restrictions capability set: native desktop Vulkan, a
    /// feature superset of the portability subset. Also what a headless build (no
    /// device) assumes. Every portability feature is available; no portability
    /// extension is present. Limits are left permissive (`min_image_count` 0 means
    /// "not queried", not "0 allowed").
    pub const FULL: GpuCaps = GpuCaps {
        portability_subset: false,
        triangle_fans: true,
        separate_stencil_mask_ref: true,
        image_view_format_reinterpretation: true,
        image_view_format_swizzle: true,
        constant_alpha_color_blend_factors: true,
        point_polygons: true,
        surface_min_image_count: 0,
        surface_max_image_count: 0,
        sampler_anisotropy: true,
        fill_mode_non_solid: true,
        independent_blend: true,
    };
}

impl Default for GpuCaps {
    /// Defaults to [`GpuCaps::FULL`] — native Vulkan / "no portability
    /// restrictions". A device query overwrites this; absent a query (headless),
    /// assuming full is the safe superset for the *seam* (nothing gates on it yet).
    fn default() -> Self {
        GpuCaps::FULL
    }
}

/// The present/sync surface the PM4 executor (`ps4-gnm`, phase 3) drives when a
/// `SubmitAndFlip` command crosses the command stream (doc-2 §3 thread boundary).
///
/// The Vulkan device lives on the display thread; the executor runs on the guest
/// thread inside the `libSceGnmDriver` submit handler and must never touch Vulkan.
/// So — unlike [`GpuBackend`], which is the display thread's own handle to the
/// device — this trait is the *guest-thread* seam: its sole impl (over the
/// `GpuManager` channel in `ps4-gpu`) ships the flip across the existing crossbeam
/// channel and blocks on the block-until-vsync handshake `videoout` already uses
/// (doc-2 §3: "SubmitAndFlip reuses the current block-until-vsync handshake").
/// Keeping it here, next to `GpuBackend`, keeps `ps4-gnm` Vulkan-free: it names
/// only this trait, never `GpuManager`/`ash::vk`.
///
/// GPU→CPU sync (EOP/EOS labels, doc-2 §C2) is *not* a method here: for phase 3 it
/// is a synchronous write into identity-mapped guest memory the executor performs
/// itself, so it needs no thread crossing.
pub trait PresentSink: Send + Sync {
    /// Present the frame the current `SubmitAndFlip` names. Reuses the softgpu
    /// present path end-to-end: the impl sends a flip over the display
    /// channel and blocks until the display thread has presented it.
    ///
    /// `vo_handle`/`buf_idx` are the videoout handle and scanout buffer index the
    /// guest's `sceGnmSubmitAndFlipCommandBuffers` named (its arg7/arg8). A
    /// double-buffered title renders into `buf_idx` and flips it on screen, so the
    /// impl must present *that* buffer's `(vo_handle, buf_idx)` registration — not a
    /// fixed one — or the just-drawn frame never scans out.
    fn submit_and_flip(&self, vo_handle: i32, buf_idx: u32);

    /// Ship a recorded [`BackendCmd`] list to the display thread to replay against
    /// the backend (doc-2 §3: the guest-thread executor emits a data list; the
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

/// Whether the guest has ever COLLECTED a GPU completion from an event queue. Set once,
/// never cleared: a title that consumes completions that way keeps doing so for the run.
static COMPLETION_EVENT_REGISTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// When the first EOP submit went by, for the label watchdog below. `Mutex<Option<Instant>>`
/// rather than `OnceLock` so a test can plant a time; set once at runtime.
static FIRST_EOP_SUBMIT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// How long an unwaited title is given before we conclude it polls the EOP label. An equeue
/// title reaches its first `sceKernelWaitEqueue` within the first few frames (~50 ms for
/// Celeste), so any value comfortably above that but still a short boot hiccup works. The
/// window only ever delays a NON-equeue title's first correct frames; an equeue title is
/// already collecting completions long before it elapses, so it is never affected.
const LABEL_WATCHDOG_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Note that the guest is collecting GPU completions from an event queue. Called from
/// `sceKernelWaitEqueue` — the wait, not the registration; see [`should_write_completion_label`].
pub fn note_completion_event_registered() {
    COMPLETION_EVENT_REGISTERED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Set the equeue-collection flag explicitly, for tests that exercise both arms of the
/// label decision. The guest path only ever sets it true.
pub fn set_completion_event_registered(value: bool) {
    COMPLETION_EVENT_REGISTERED.store(value, std::sync::atomic::Ordering::Relaxed);
}

/// Record that an EOP submit happened, arming the label watchdog on the first one. Called
/// from the executor's `emit_label`.
pub fn note_eop_submit() {
    if let Ok(mut g) = FIRST_EOP_SUBMIT.lock()
        && g.is_none()
    {
        *g = Some(std::time::Instant::now());
    }
}

/// Plant the first-EOP-submit time, for tests that need the watchdog to have elapsed (or
/// not) without sleeping.
pub fn set_first_eop_submit_for_test(when: Option<std::time::Instant>) {
    if let Ok(mut g) = FIRST_EOP_SUBMIT.lock() {
        *g = when;
    }
}

/// Has the guest collected a GPU completion from an event queue? An equeue title trips this
/// within milliseconds of its first frame; a label-poller never does. The label-flush
/// watchdog reads it to know an equeue title needs no flush.
pub fn completion_event_registered() -> bool {
    COMPLETION_EVENT_REGISTERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Should the executor surface the raw EOP completion label into guest memory?
///
/// This is the crux of two titles needing OPPOSITE things from the same packet, decided so
/// that the working title is never put at risk:
///
/// * **Equeue titles** (Celeste) block in `sceKernelWaitEqueue` — 1577 waits in a 40 s run.
///   Completion reaches them from the queue, so the raw label is only gnmx's buffer-recycle
///   hint. Writing it makes gnmx skip re-recording per-draw state, collapsing textures to
///   white (task-157). These must NEVER get the label — including during boot.
/// * **Label-polling titles** (the UE4/Little Nightmares title) spin on the label in guest
///   memory as their only completion signal. Withholding it hangs the submit thread, which
///   holds a lock the whole task graph waits behind.
///
/// The trap the first attempt fell into: gating on whether the guest has *waited* is decided
/// too late. An equeue title has not waited yet during its first ~3 submits, so a
/// wait-gated write hands it the label for those frames — and three collapsed frames at boot
/// are enough for gnmx to stay collapsed forever. The screenshot proved it.
///
/// So the default is WITHHOLD, unconditionally, and the label is written only once a title
/// has *positively shown* it is a poller: no equeue completion collected, and more than
/// [`LABEL_WATCHDOG_GRACE`] elapsed since its first submit. An equeue title trips the
/// "collected a completion" flag within milliseconds, long before the grace elapses, so it
/// takes the withhold path on every frame including the first. A poller never collects, so
/// after the grace it takes the write path and un-wedges — at the cost of a ~1 s boot stall
/// that only it ever pays.
pub fn should_write_completion_label() -> bool {
    // Ever collected a completion from the queue? Then the queue is the channel; never write.
    if COMPLETION_EVENT_REGISTERED.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    // No collection yet. Withhold until the grace elapses; only then is silence proof of a
    // poller rather than an equeue title that simply has not reached its first wait.
    match FIRST_EOP_SUBMIT.lock().ok().and_then(|g| *g) {
        Some(first) => first.elapsed() >= LABEL_WATCHDOG_GRACE,
        None => false,
    }
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
/// aliases (doc-2 §5 "map RT to the videoout fb when the base matches a registered
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

/// The guest free/unmap → resource-cache invalidation seam (doc-2 §8). The kernel memory
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
    //! (doc-2 §C3/§C4, AC #1). Assert each command's fields against hand-reasoned expected
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
            address_mode_u: SamplerAddressMode::Repeat,
            address_mode_v: SamplerAddressMode::Repeat,
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
                assert_eq!(desc.address_mode_u, SamplerAddressMode::Repeat);
                assert_eq!(desc.address_mode_v, SamplerAddressMode::Repeat);
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
        // A textured pipeline carries one TextureBinding per texture the PS samples so the
        // backend adds that many combined image-sampler descriptors; the vertex-pull /
        // embedded paths carry an empty list. A PS that mixes a register-resident T# with
        // a memory-resident one declares TWO (task-199).
        let cmd = BackendCmd::CreatePipeline {
            id: PipelineId(1),
            vs_spirv: Arc::from(vec![0u32]),
            ps_spirv: Arc::from(vec![0u32]),
            key: Box::new(PipelineKey::default()),
            target: TargetDesc::default(),
            vertex_storage: Vec::new(),
            push_constants: None,
            textures: vec![
                TextureBinding { set: 0, binding: 1 },
                TextureBinding { set: 0, binding: 7 },
            ],
            const_storage: None,
            const_storage_fragment: None,
        };
        match cmd {
            BackendCmd::CreatePipeline { textures, .. } => {
                assert_eq!(
                    textures,
                    vec![
                        TextureBinding { set: 0, binding: 1 },
                        TextureBinding { set: 0, binding: 7 },
                    ]
                );
            }
            other => panic!("expected CreatePipeline, got {other:?}"),
        }
    }

    #[test]
    fn create_render_target_carries_its_fields_and_has_no_upload() {
        // A CreateRenderTarget names an id + extent + color format. Unlike CreateImage there
        // is deliberately NO paired upload variant — the GPU fills a render target, so the
        // command list carries only the create. Hand-reasoned expected shape.
        let cmd = BackendCmd::CreateRenderTarget {
            id: ResourceId(5),
            width: 128,
            height: 64,
            format: ColorFormat::R8G8B8A8Unorm,
        };
        match cmd {
            BackendCmd::CreateRenderTarget {
                id,
                width,
                height,
                format,
            } => {
                assert_eq!(id, ResourceId(5));
                assert_eq!(width, 128);
                assert_eq!(height, 64);
                assert_eq!(format, ColorFormat::R8G8B8A8Unorm);
            }
            other => panic!("expected CreateRenderTarget, got {other:?}"),
        }
    }

    #[test]
    fn readback_render_target_names_id_and_guest_range() {
        let cmd = BackendCmd::ReadbackRenderTarget {
            id: ResourceId(5),
            addr: 0xC000_0000,
            size: 0x2000,
            pitch: 1024,
            tiling: Tiling::Tiled { tile_mode_index: 8 },
        };
        match cmd {
            BackendCmd::ReadbackRenderTarget {
                id,
                addr,
                size,
                pitch,
                tiling,
            } => {
                assert_eq!(id, ResourceId(5));
                assert_eq!(addr, 0xC000_0000);
                assert_eq!(size, 0x2000);
                // The guest surface geometry rides along with the command (task-181): the
                // backend cannot pack a decodable readback from the content extent alone.
                assert_eq!(pitch, 1024);
                assert_eq!(tiling, Tiling::Tiled { tile_mode_index: 8 });
            }
            other => panic!("expected ReadbackRenderTarget, got {other:?}"),
        }
    }

    #[test]
    fn readback_policy_default_and_from_str_resolve() {
        // Default + the off-family strings resolve Off; anything else resolves All (doc-2
        // §8.5). `from_str` is env-free so the parse is unit-testable deterministically; the
        // readback *emit* itself is step 5, so only the policy resolution is exercised here.
        assert_eq!(ReadbackPolicy::default(), ReadbackPolicy::Off);
        for off in ["", "off", "OFF", " 0 ", "0", "false", "False"] {
            assert_eq!(
                ReadbackPolicy::parse(off),
                ReadbackPolicy::Off,
                "{off:?} must resolve Off"
            );
        }
        for on in ["1", "all", "on", "true", "yes"] {
            assert_eq!(
                ReadbackPolicy::parse(on),
                ReadbackPolicy::All,
                "{on:?} must resolve All"
            );
        }
    }

    #[test]
    fn gpu_caps_default_is_full() {
        // task-136: absent a device query (native / headless) GpuCaps reflects
        // "full" — a feature superset of the portability subset with no
        // restrictions. This is the invariant the seam relies on: nothing gates on
        // caps yet, so a missing/native query must never look MORE restricted than
        // MoltenVK.
        let caps = GpuCaps::default();
        assert_eq!(caps, GpuCaps::FULL);
        assert!(!caps.portability_subset);
        assert!(caps.triangle_fans);
        assert!(caps.separate_stencil_mask_ref);
        assert!(caps.image_view_format_reinterpretation);
        assert!(caps.image_view_format_swizzle);
        assert!(caps.constant_alpha_color_blend_factors);
        assert!(caps.point_polygons);
        assert!(caps.sampler_anisotropy);
        assert!(caps.fill_mode_non_solid);
        assert!(caps.independent_blend);
    }

    /// The GFX6 `CB_BLENDn_CONTROL` field split, anchored on the two live Celeste controls.
    /// This decode is shared by the Vulkan pipeline and the GPU-state snapshot, so a wrong
    /// bit position here would make the snapshot's decoded blend and the pipeline it claims
    /// to describe disagree — silently.
    #[test]
    fn blend_key_fields_split_the_gfx6_control_word() {
        // Premultiplied over: SRC=ONE(1), DST=ONE_MINUS_SRC_ALPHA(5), ADD(0).
        let premult = BlendKey {
            enable: true,
            control: 0x4501_0501,
            write_mask: 0xF,
        }
        .fields();
        assert_eq!(premult.color_src, 1);
        assert_eq!(premult.color_dst, 5);
        assert_eq!(premult.color_comb, 0);
        // SEPARATE_ALPHA_BLEND clear → alpha MIRRORS colour rather than reading raw bits.
        assert!(!premult.separate_alpha);
        assert_eq!(premult.alpha_src, premult.color_src);
        assert_eq!(premult.alpha_dst, premult.color_dst);
        assert_eq!(blend_factor_name(premult.color_src), "ONE");
        assert_eq!(blend_factor_name(premult.color_dst), "ONE_MINUS_SRC_ALPHA");
        assert_eq!(blend_op_name(premult.color_comb), "ADD");

        // Additive: SRC=SRC_ALPHA(4), DST=ONE(1), ADD.
        let additive = BlendKey {
            enable: true,
            control: 0x4104_0104,
            write_mask: 0xF,
        }
        .fields();
        assert_eq!(blend_factor_name(additive.color_src), "SRC_ALPHA");
        assert_eq!(blend_factor_name(additive.color_dst), "ONE");

        // SEPARATE_ALPHA_BLEND set, with alpha fields distinct from colour: values >= 8 and a
        // COMB_FCN that only decodes right at shift 21 catch a wrong shift or a truncated mask.
        let separate = BlendKey {
            enable: true,
            control: (1 << 29) | (8 << 24) | (3 << 21) | (8 << 16) | 0x0501,
            write_mask: 0xF,
        }
        .fields();
        assert!(separate.separate_alpha);
        assert_eq!(separate.alpha_src, 8);
        assert_eq!(separate.alpha_comb, 3);
        assert_eq!(separate.alpha_dst, 8);
        assert_eq!(blend_op_name(separate.alpha_comb), "MAX");
        // An unmodeled enum reads as unknown, never as the value the backend falls back to.
        assert_eq!(blend_factor_name(31), "?");
        assert_eq!(blend_op_name(7), "?");
    }
}
