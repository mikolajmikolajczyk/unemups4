//! V# buffer descriptors + the user-SGPR binding model (doc-4 §C4, §5).
//!
//! GCN shaders do not use Vulkan-style descriptor sets: they **load resource
//! descriptors from memory** through pointers the driver preloads into *user SGPRs*
//! (doc-4 §C4). This module implements the vertex/constant-buffer half of that model:
//!
//! 1. **User-SGPR block** — the `SPI_SHADER_USER_DATA_VS_*` / `_PS_*` SH registers are
//!    the 16 user SGPRs the driver preloaded for a stage. [`UserData::from_regs`] reads
//!    the block back out of the shadow register file.
//! 2. **V# decode** — a 128-bit V# (buffer resource) is four little-endian dwords in
//!    guest memory (`base`, `stride`, `num_records`, format/swizzle). [`decode_v_sharp`]
//!    turns those bytes into a typed [`BufferDesc`], including the `dfmt`/`nfmt`/dst-sel
//!    fields word3 carries. The decode is total: any 16-byte input decodes, and the
//!    *validity* of the result is a separate [`BufferDesc::is_null`] check.
//! 3. **Draw-time derivation** — given a stage's [`UserData`] and a [`FetchLayout`]
//!    (which user-SGPR slots hold V# pointers, from the shader's I/O layout),
//!    [`derive_buffer_ranges`] reads each V# from memory and yields the
//!    `(addr, size, layout)` triples the [`ResourceCache`](crate::cache::ResourceCache)
//!    consumes, plus a [`VertexInputDesc`] — the vertex-input part of the pipeline key.
//!
//! ## Untrusted pointers
//!
//! A V#-descriptor pointer comes out of a guest-programmed register, so it is
//! **untrusted**. Every read of descriptor bytes goes through the bounded/ranged read
//! seam ([`BoundedRead`]), never a bare identity view: a pointer near an unmapped page
//! is a clean [`VbufError`], not an over-read into host memory.
//!
//! ## Scope (doc-4 §C4)
//!
//! The **vertex/const-buffer** slice: V# (buffer) descriptors, plus the sampled-texture
//! T# (image) / S# (sampler) decode a PS `image_sample` needs ([`decode_t_sharp`] /
//! [`decode_s_sharp`] / [`derive_texture`]). Fetch-shader emulation stays deferred.
//! Nothing here uploads — it produces the addr/size/layout triples (and T#/S#) the cache
//! turns into backend commands.

use ps4_core::bounded_read::BoundedRead;
use ps4_core::gpu::VertexFormat;

use crate::cache::ResLayout;
use crate::pm4::opcodes::sh_reg;
use crate::shader::source::Stage;
use crate::state::GpuState;

/// Size of a single V# (buffer resource) descriptor in bytes: four 32-bit dwords.
pub const V_SHARP_SIZE: usize = 16;

/// The GCN `BUF_DATA_FORMAT` field (V# word3 bits [18:15]) — how many components a
/// buffer element has and the width of each. Only the formats the corpus /
/// `buffer_load_format_*` fetch path exercises are named; any other 4-bit value is kept
/// as [`DataFormat::Other`] rather than rejected, so an unusual-but-well-formed V# still
/// decodes (validity is a separate [`BufferDesc::is_null`] question).
///
/// Values are the standard AMD Sea Islands `BUF_DATA_FORMAT_*` enumeration (mirrored
/// from shadPS4 `video_core/amdgpu/resource.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataFormat {
    /// `0` — an invalid/unbound descriptor (`BUF_DATA_FORMAT_INVALID`).
    Invalid,
    /// `1` — one 8-bit component.
    Format8,
    /// `2` — one 16-bit component.
    Format16,
    /// `3` — two 8-bit components (`_8_8`).
    Format8_8,
    /// `4` — one 32-bit component (`BUF_DATA_FORMAT_32`).
    Format32,
    /// `5` — two 16-bit components (`_16_16`).
    Format16_16,
    /// `10` — four 8-bit components (`_8_8_8_8`).
    Format8_8_8_8,
    /// `11` — two 32-bit components (`_32_32`).
    Format32_32,
    /// `12` — four 16-bit components (`_16_16_16_16`, used for half4 attributes).
    Format16_16_16_16,
    /// `13` — three 32-bit components (`_32_32_32`).
    Format32_32_32,
    /// `14` — four 32-bit components (`_32_32_32_32` — the `buffer_load_format_xyzw`
    /// vec4 the corpus VS fetches).
    Format32_32_32_32,
    /// Any recognized-encoding-but-unmodeled `dfmt` value, kept verbatim.
    Other(u8),
}

impl DataFormat {
    /// Decode the 4-bit `BUF_DATA_FORMAT` field (AMD Sea Islands enumeration).
    fn from_bits(dfmt: u8) -> DataFormat {
        match dfmt {
            0 => DataFormat::Invalid,
            1 => DataFormat::Format8,
            2 => DataFormat::Format16,
            3 => DataFormat::Format8_8,
            4 => DataFormat::Format32,
            5 => DataFormat::Format16_16,
            10 => DataFormat::Format8_8_8_8,
            11 => DataFormat::Format32_32,
            12 => DataFormat::Format16_16_16_16,
            13 => DataFormat::Format32_32_32,
            14 => DataFormat::Format32_32_32_32,
            other => DataFormat::Other(other),
        }
    }

    /// Number of components an element of this format carries (1..=4), or `None` for a
    /// format whose component count this table does not model.
    pub fn components(self) -> Option<u32> {
        Some(match self {
            DataFormat::Invalid => return None,
            DataFormat::Format8 | DataFormat::Format16 | DataFormat::Format32 => 1,
            DataFormat::Format8_8 | DataFormat::Format16_16 | DataFormat::Format32_32 => 2,
            DataFormat::Format32_32_32 => 3,
            DataFormat::Format8_8_8_8
            | DataFormat::Format16_16_16_16
            | DataFormat::Format32_32_32_32 => 4,
            DataFormat::Other(_) => return None,
        })
    }
}

/// The GCN `BUF_NUM_FORMAT` field (V# word3 bits [14:12]) — how a component's raw bits
/// are interpreted (unorm / snorm / float / (u)int, …). Only the values the vertex-fetch
/// corpus uses are named; anything else is [`NumFormat::Other`]. Values are the standard
/// AMD Sea Islands `BUF_NUM_FORMAT_*` enumeration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumFormat {
    /// `0` — unsigned normalized `[0,1]`.
    Unorm,
    /// `1` — signed normalized `[-1,1]`.
    Snorm,
    /// `4` — unsigned integer.
    Uint,
    /// `5` — signed integer.
    Sint,
    /// `7` — 32-bit float (`BUF_NUM_FORMAT_FLOAT`; the corpus vec4 position).
    Float,
    /// Any recognized-encoding-but-unmodeled `nfmt` value, kept verbatim.
    Other(u8),
}

impl NumFormat {
    /// Decode the 3-bit `BUF_NUM_FORMAT` field.
    fn from_bits(nfmt: u8) -> NumFormat {
        match nfmt {
            0 => NumFormat::Unorm,
            1 => NumFormat::Snorm,
            4 => NumFormat::Uint,
            5 => NumFormat::Sint,
            7 => NumFormat::Float,
            other => NumFormat::Other(other),
        }
    }
}

/// A decoded 128-bit V# (buffer resource, doc-4 §C4). The full descriptor: the fetch
/// base/stride/num-records the [`ResourceCache`](crate::cache::ResourceCache) keys on,
/// plus the word3 format/swizzle fields a later pipeline-key stage needs to describe the
/// vertex attribute. Value-typed and comparable so it can seed a pipeline key.
///
/// The three-word base/stride/num_records layout matches the partial decoder in the GCN
/// interpreter (`ps4-gcn`), so the state-side derivation and the shader-side fetch agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferDesc {
    /// 48-bit guest base address (word0 | word1[15:0] << 32).
    pub base: u64,
    /// Per-element stride in bytes (word1[29:16], 14 bits). `0` for a tightly-packed
    /// linear (non-indexed) fetch.
    pub stride: u32,
    /// Element count (word2). `0` is a valid empty / unbound descriptor.
    pub num_records: u32,
    /// `BUF_DATA_FORMAT` — element component count / width.
    pub dfmt: DataFormat,
    /// `BUF_NUM_FORMAT` — component interpretation.
    pub nfmt: NumFormat,
    /// Destination swizzle (`dst_sel_{x,y,z,w}`, word3 bits [2:0][5:3][8:6][11:9]); each
    /// selects 0 / 1 / the component that maps to x/y/z/w (`SQ_SEL_*`). Carried verbatim
    /// so a later pipeline-key stage can reproduce the attribute mapping.
    pub dst_sel: [u8; 4],
}

impl BufferDesc {
    /// The total byte span the fetch may touch: `stride * num_records` when strided,
    /// else `element_size * num_records` for a tightly-packed buffer. This is the `size`
    /// half of the `(addr, size, layout)` triple the cache keys on. Saturating so a
    /// hostile `stride * num_records` can never wrap to a small span.
    pub fn byte_span(&self) -> u64 {
        let per_element = if self.stride != 0 {
            u64::from(self.stride)
        } else {
            u64::from(self.element_size())
        };
        per_element.saturating_mul(u64::from(self.num_records))
    }

    /// The byte size of one element implied by `dfmt` (used when `stride == 0`). `0` for
    /// a format whose size this table does not model — a caller that gets a zero span
    /// treats the descriptor as unusable (a clean defer, not a fetch of the whole space).
    fn element_size(&self) -> u32 {
        // Component width in bytes, times component count. Only the modeled formats
        // yield a size; anything else returns 0.
        let (comp_bytes, comps) = match self.dfmt {
            DataFormat::Format8 | DataFormat::Format8_8 | DataFormat::Format8_8_8_8 => {
                (1, self.dfmt.components().unwrap_or(0))
            }
            DataFormat::Format16 | DataFormat::Format16_16 | DataFormat::Format16_16_16_16 => {
                (2, self.dfmt.components().unwrap_or(0))
            }
            DataFormat::Format32
            | DataFormat::Format32_32
            | DataFormat::Format32_32_32
            | DataFormat::Format32_32_32_32 => (4, self.dfmt.components().unwrap_or(0)),
            DataFormat::Invalid | DataFormat::Other(_) => (0, 0),
        };
        comp_bytes * comps
    }

    /// Whether this descriptor is null / unbound: a zero base, or an invalid data format,
    /// or a span that resolves to zero bytes. A null V# is not decoded into a cache range
    /// — the draw defers it cleanly (AC #3), rather than uploading a zero-length or
    /// wild-pointer resource.
    pub fn is_null(&self) -> bool {
        self.base == 0 || self.dfmt == DataFormat::Invalid || self.byte_span() == 0
    }
}

/// Decode a 128-bit V# from its four little-endian dwords (doc-4 §C4). Total: any
/// 16-byte input decodes to a [`BufferDesc`]; call [`BufferDesc::is_null`] to test
/// usability. The bit layout is the standard AMD Sea Islands buffer resource
/// (shadPS4 `video_core/amdgpu/resource.h`):
///
/// | Word | Bits    | Field                                   |
/// |------|---------|-----------------------------------------|
/// | 0    | [31:0]  | `base_address` low                      |
/// | 1    | [15:0]  | `base_address` high (bits 47:32)        |
/// | 1    | [29:16] | `stride` (bytes)                        |
/// | 2    | [31:0]  | `num_records`                           |
/// | 3    | [2:0]   | `dst_sel_x`                             |
/// | 3    | [5:3]   | `dst_sel_y`                             |
/// | 3    | [8:6]   | `dst_sel_z`                             |
/// | 3    | [11:9]  | `dst_sel_w`                             |
/// | 3    | [14:12] | `nfmt` (`BUF_NUM_FORMAT`, 3 bits)       |
/// | 3    | [18:15] | `dfmt` (`BUF_DATA_FORMAT`, 4 bits)      |
pub fn decode_v_sharp(words: [u32; 4]) -> BufferDesc {
    let [w0, w1, w2, w3] = words;
    let base = u64::from(w0) | (u64::from(w1 & 0xFFFF) << 32);
    let stride = (w1 >> 16) & 0x3FFF;
    let num_records = w2;
    // word3: dst_sel_{x,y,z,w} in [2:0][5:3][8:6][11:9]; nfmt (3 bits) [14:12]; dfmt (4
    // bits) [18:15]. On GFX6/7 the format fields sit directly above dst_sel — a real
    // hardware-laid descriptor decodes wrong if these are shifted up by the dst_sel span.
    let dst_sel = [
        (w3 & 0x7) as u8,
        ((w3 >> 3) & 0x7) as u8,
        ((w3 >> 6) & 0x7) as u8,
        ((w3 >> 9) & 0x7) as u8,
    ];
    let nfmt = NumFormat::from_bits(((w3 >> 12) & 0x7) as u8);
    let dfmt = DataFormat::from_bits(((w3 >> 15) & 0xF) as u8);
    BufferDesc {
        base,
        stride,
        num_records,
        dfmt,
        nfmt,
        dst_sel,
    }
}

/// Size of a T# (image resource) descriptor in bytes: eight 32-bit dwords (256-bit).
pub const T_SHARP_SIZE: usize = 32;
/// Size of an S# (sampler) descriptor in bytes: four 32-bit dwords (128-bit).
pub const S_SHARP_SIZE: usize = 16;

/// A decoded T# (256-bit image resource, doc-4 §C3/§C4). Carries the fields the linear-
/// RGBA8 sampled-texture upload path needs: the texel base address, extent, and the
/// `dfmt`/`nfmt`/tiling that key the cache entry. Matches the GFX6/7 image-descriptor
/// layout the interpreter's sampling oracle reads (`ps4_gcn` `decode_t_sharp`), so the
/// bytes the cache uploads and the bytes the oracle samples are the same texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureDesc {
    /// Guest base address of the texel data (word0 << 8).
    pub base: u64,
    /// Texel width (word2[13:0] + 1).
    pub width: u32,
    /// Texel height (word2[27:14] + 1).
    pub height: u32,
    /// `dfmt` (word1[25:20]).
    pub dfmt: u8,
    /// `nfmt` (word1[29:26]).
    pub nfmt: u8,
    /// Tiling index (word3[22:20]); 0 = linear, non-zero = tiled (detiled on upload).
    pub tiling_index: u8,
}

impl TextureDesc {
    /// Whether this T# is unbound/degenerate (null base or zero extent) and the draw
    /// must defer rather than sample it.
    pub fn is_null(&self) -> bool {
        self.base == 0 || self.width == 0 || self.height == 0
    }

    /// Byte span of the texel data for this texture. Linear R8G8B8A8 is
    /// `width * height * 4`; tiled surfaces round each extent up to whole 8×8 micro-
    /// tiles (the seam the cache detiler reads). Saturating so a hostile extent can't
    /// wrap the range the bounded seam validates.
    pub fn byte_span(&self) -> u64 {
        if self.tiling_index == 0 {
            (self.width as u64)
                .saturating_mul(self.height as u64)
                .saturating_mul(4)
        } else {
            let tiles_w = self.width.div_ceil(8) as u64;
            let tiles_h = self.height.div_ceil(8) as u64;
            tiles_w
                .saturating_mul(tiles_h)
                .saturating_mul(64)
                .saturating_mul(4)
        }
    }
}

/// A decoded S# (128-bit sampler, doc-4 §C4). Only the filter selector the portable
/// subset honors is modeled (no anisotropy/LOD/border); matches the interpreter's
/// `decode_s_sharp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SamplerState {
    /// `true` = bilinear filtering, `false` = point/nearest (word2[20]).
    pub bilinear: bool,
}

/// Decode a 256-bit T# (image resource) from eight little-endian dwords (doc-4 §C3).
/// Total: any 32-byte input decodes; validity is the separate [`TextureDesc::is_null`]
/// check. Layout (GFX6/7 image descriptor):
/// - word0 = base[39:8]; word1[7:0] = base[47:40] (guest base = (word0<<8) | (w1[7:0]<<40))
/// - word1[25:20] = dfmt, word1[29:26] = nfmt
/// - word2[13:0] = width - 1; word2[27:14] = height - 1
/// - word3[22:20] = tiling index (0 = linear)
///
/// HLE NOTE: the base is treated as 48-bit (like the V#) rather than the hardware's
/// 40-bit, so an identity-mapped 48-bit host address round-trips. `word1[7:0]` (which
/// GFX6/7 uses for `min_lod` fraction bits) carries base[47:40] in this HLE model; the
/// subset does not use mips, so nothing is lost.
pub fn decode_t_sharp(words: [u32; 8]) -> TextureDesc {
    let [w0, w1, w2, w3, ..] = words;
    TextureDesc {
        base: (u64::from(w0) << 8) | (u64::from(w1 & 0xFF) << 40),
        width: (w2 & 0x3FFF) + 1,
        height: ((w2 >> 14) & 0x3FFF) + 1,
        dfmt: ((w1 >> 20) & 0x3F) as u8,
        nfmt: ((w1 >> 26) & 0xF) as u8,
        tiling_index: ((w3 >> 20) & 0x1F) as u8,
    }
}

/// Decode a 128-bit S# (sampler) from four little-endian dwords (doc-4 §C4). Only the
/// filter select (word2[20]) is read; the rest is not consulted in the subset.
pub fn decode_s_sharp(words: [u32; 4]) -> SamplerState {
    SamplerState {
        bilinear: (words[2] >> 20) & 1 == 1,
    }
}

/// The 16 user-SGPR words the driver preloaded for one stage (doc-4 §C4). These are the
/// `SPI_SHADER_USER_DATA_*_*` SH registers read back verbatim; a shader's [`FetchLayout`]
/// says which of these words is a V#-descriptor pointer (or a pointer pair).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserData {
    words: [u32; sh_reg::USER_DATA_SLOTS as usize],
}

impl UserData {
    /// Read a stage's user-SGPR block out of the SH-bank shadow register file. An
    /// un-written slot reads back as `0` (the driver only programs the slots the shader
    /// declares), so a missing V# pointer decodes to a null descriptor downstream.
    pub fn from_regs(state: &GpuState, stage: Stage) -> UserData {
        let base = match stage {
            Stage::Vertex => sh_reg::SPI_SHADER_USER_DATA_VS_0,
            Stage::Pixel => sh_reg::SPI_SHADER_USER_DATA_PS_0,
        };
        let mut words = [0u32; sh_reg::USER_DATA_SLOTS as usize];
        for (i, w) in words.iter_mut().enumerate() {
            *w = state.sh_regs.get(base + i as u32).unwrap_or(0);
        }
        UserData { words }
    }

    /// The raw user-SGPR word at `slot` (0..16), or `None` if `slot` is out of range.
    pub fn slot(&self, slot: usize) -> Option<u32> {
        self.words.get(slot).copied()
    }

    /// A 64-bit pointer formed from the user-SGPR pair `[slot, slot+1]` (low word first,
    /// GCN's little-endian SGPR-pair convention). `None` if the pair runs past the block.
    pub fn ptr_pair(&self, slot: usize) -> Option<u64> {
        let lo = self.slot(slot)?;
        let hi = self.slot(slot + 1)?;
        Some(u64::from(lo) | (u64::from(hi) << 32))
    }
}

/// How a V# a stage fetches is reached from its user SGPRs (doc-4 §C4). This is the
/// vertex/const-buffer slice of the shader's I/O layout: for each buffer the shader
/// reads, which user-SGPR pair holds the *pointer to the V#*, at what offset in the
/// descriptor set, and how the cache should key the resulting range.
///
/// Phase 3.5's `EmbeddedShaderProvider` has no such layout (its bindings are fixed). The
/// GCN provider (phase 4) derives one from the recompiled module's
/// [`BufferBinding`](ps4_gcn::BufferBinding); this type is what the executor consumes,
/// so the executor never learns a shader's kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchLayout {
    /// One entry per buffer the shader fetches, in binding order.
    pub buffers: Vec<BufferSlot>,
}

/// One buffer a shader fetches, and how to reach its V# (doc-4 §C4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferSlot {
    /// The user-SGPR slot holding the low word of the pointer to the descriptor set that
    /// contains this V# (the pointer occupies `[user_sgpr, user_sgpr+1]`). Matches the
    /// corpus ABI where `s[2:3]` is the descriptor-set pointer.
    pub user_sgpr: usize,
    /// Byte offset of this V# within the descriptor set the pointer names. `0` for the
    /// first (or only) descriptor.
    pub desc_offset: u64,
    /// How the cache should key the range this V# points at.
    pub layout: ResLayout,
}

/// A resolved buffer range plus the descriptor it came from — the `(addr, size, layout)`
/// triple the [`ResourceCache`](crate::cache::ResourceCache) consumes, with the decoded
/// [`BufferDesc`] retained so a later pipeline-key stage has the format/swizzle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferRange {
    /// Guest base address of the buffer (from the V#).
    pub addr: u64,
    /// Byte span the fetch may touch ([`BufferDesc::byte_span`]).
    pub size: u64,
    /// Cache key layout for this range.
    pub layout: ResLayout,
    /// The decoded descriptor (retained for the pipeline-input part).
    pub desc: BufferDesc,
}

impl BufferRange {
    /// The `(addr, size, layout)` triple the cache keys on.
    pub fn key_triple(&self) -> (u64, u64, ResLayout) {
        (self.addr, self.size, self.layout)
    }
}

/// The vertex-input part of the pipeline key (doc-4 §4, §C4): the ordered attribute
/// formats a draw's vertex fetch produces, derived from the vertex-buffer V#s. The
/// full `PipelineKey` (shader identity + render-target state) lands with the pipeline
/// path (later task); this is the vertex-input slice that part will embed, kept
/// value-comparable so it can hash into that key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VertexInputDesc {
    /// One entry per vertex-buffer binding, in fetch order.
    pub attributes: Vec<VertexAttribute>,
}

/// One vertex attribute derived from a vertex-buffer V# (doc-4 §C4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexAttribute {
    /// Element stride in bytes (V# `stride`).
    pub stride: u32,
    /// Component count / width (V# `dfmt`).
    pub dfmt: DataFormat,
    /// Component interpretation (V# `nfmt`).
    pub nfmt: NumFormat,
    /// Destination swizzle (`dst_sel`).
    pub dst_sel: [u8; 4],
}

impl VertexAttribute {
    fn from_desc(d: &BufferDesc) -> VertexAttribute {
        VertexAttribute {
            stride: d.stride,
            dfmt: d.dfmt,
            nfmt: d.nfmt,
            dst_sel: d.dst_sel,
        }
    }

    /// Fold this attribute's `dfmt`/`nfmt` into the Vulkan-free host [`VertexFormat`] the
    /// pipeline key carries to the backend (doc-4 §C4). Only the corpus + common
    /// combinations are modeled; any other pairing yields [`VertexFormat::Unsupported`],
    /// so the draw defers rather than the backend guessing a `vk::Format` that would
    /// mismatch the SPIR-V input decoration. The `dfmt`/`nfmt` → `vk::Format` half of the
    /// mapping lives in the backend (`ps4-gpu`); this half stays Vulkan-free.
    pub fn to_vertex_format(&self) -> VertexFormat {
        match (self.dfmt, self.nfmt) {
            (DataFormat::Format32, NumFormat::Float) => VertexFormat::R32Sfloat,
            (DataFormat::Format32_32, NumFormat::Float) => VertexFormat::R32G32Sfloat,
            (DataFormat::Format32_32_32, NumFormat::Float) => VertexFormat::R32G32B32Sfloat,
            (DataFormat::Format32_32_32_32, NumFormat::Float) => VertexFormat::R32G32B32A32Sfloat,
            (DataFormat::Format32, NumFormat::Uint) => VertexFormat::R32Uint,
            (DataFormat::Format32_32_32_32, NumFormat::Uint) => VertexFormat::R32G32B32A32Uint,
            (DataFormat::Format32, NumFormat::Sint) => VertexFormat::R32Sint,
            (DataFormat::Format32_32_32_32, NumFormat::Sint) => VertexFormat::R32G32B32A32Sint,
            (DataFormat::Format8_8_8_8, NumFormat::Unorm) => VertexFormat::R8G8B8A8Unorm,
            (DataFormat::Format16_16, NumFormat::Unorm) => VertexFormat::R16G16Unorm,
            _ => VertexFormat::Unsupported,
        }
    }
}

/// Why deriving a draw's buffer ranges yielded nothing usable for a given slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VbufError {
    /// A user-SGPR slot the layout named runs past the 16-word block.
    SlotOutOfRange(usize),
    /// The user-SGPR pointer (or `desc_offset`) is null / would overflow — no descriptor
    /// set to read.
    NullPointer,
    /// Reading the 16-byte V# from guest memory faulted (unmapped / straddling a
    /// mapping) through the bounded seam — never an over-read.
    MemoryFault,
    /// The V# decoded but is null / unbound ([`BufferDesc::is_null`]).
    NullDescriptor,
}

/// The result of deriving one draw's referenced buffer ranges (doc-4 §C4, §5).
///
/// Total and non-panicking: a malformed or null V# is *dropped* from `ranges` (its slot
/// index recorded in `deferred`), not a hard error — a draw with one bad binding still
/// contributes its good ones. AC #2 reads `ranges`; AC #3 reads `deferred`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrawBuffers {
    /// The usable `(addr, size, layout)` ranges + descriptors, in binding order.
    pub ranges: Vec<BufferRange>,
    /// Per-slot defer reasons for the bindings that resolved to nothing usable.
    pub deferred: Vec<(usize, VbufError)>,
    /// The vertex-input part of the pipeline key, over the [`ResLayout::VertexBuf`]
    /// ranges only (const buffers do not shape vertex input).
    pub vertex_input: VertexInputDesc,
}

/// Derive the buffer ranges a draw references (doc-4 §C4, §5). For each buffer the
/// shader fetches (per `layout`), read its V# pointer out of the stage's [`UserData`],
/// decode the 128-bit V# from guest memory through the bounded seam, and — if usable —
/// yield its `(addr, size, layout)` triple. A malformed / null binding is deferred
/// cleanly, never fatal (AC #3).
///
/// `mem` is the bounded/ranged read seam: the V#-descriptor pointer is register-derived
/// and untrusted, so every descriptor read is range-validated (never a bare identity
/// over-read).
pub fn derive_buffer_ranges(
    user: &UserData,
    layout: &FetchLayout,
    mem: &(impl BoundedRead + ?Sized),
) -> DrawBuffers {
    let mut out = DrawBuffers::default();
    for (i, slot) in layout.buffers.iter().enumerate() {
        match resolve_slot(user, slot, mem) {
            Ok(range) => {
                if range.layout == ResLayout::VertexBuf {
                    out.vertex_input
                        .attributes
                        .push(VertexAttribute::from_desc(&range.desc));
                }
                out.ranges.push(range);
            }
            Err(e) => out.deferred.push((i, e)),
        }
    }
    out
}

/// How a PS reaches its sampled texture's T#/S# from its user SGPRs (doc-4 §C4). Like
/// [`BufferSlot`], but for the combined image-sampler: a user-SGPR pair points at a
/// descriptor set holding the T# at `t_offset` and the S# at `s_offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureSlot {
    /// User-SGPR slot holding the low word of the descriptor-set pointer (pair
    /// `[user_sgpr, user_sgpr+1]`). The corpus PS ABI uses `s[0:1]`.
    pub user_sgpr: usize,
    /// Byte offset of the T# within the descriptor set.
    pub t_offset: u64,
    /// Byte offset of the S# within the descriptor set.
    pub s_offset: u64,
}

/// The corpus PS's texture ABI: the descriptor-set pointer is in `s[0:1]`, the T# at
/// offset 0, the S# right after it (offset 32). A recompiled PS with a sampler binding
/// resolves through this fixed slot (mirroring the VS's fixed `s[2:3]` V# ABI).
pub const CORPUS_TEXTURE_SLOT: TextureSlot = TextureSlot {
    user_sgpr: 0,
    t_offset: 0,
    s_offset: T_SHARP_SIZE as u64,
};

/// A resolved sampled-texture binding: the decoded T# + S# a draw samples (doc-4 §C4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureBindingRange {
    /// The decoded image descriptor (base/extent/format/tiling).
    pub texture: TextureDesc,
    /// The decoded sampler state (filter).
    pub sampler: SamplerState,
}

/// Resolve a PS's T#/S# through the bounded seam (doc-4 §C4). Reads the descriptor-set
/// pointer from the PS [`UserData`], fetches the 256-bit T# and 128-bit S# (each range-
/// validated — the pointer is register-derived and untrusted), and decodes them. A null
/// pointer, a faulting read, or a null/degenerate T# defers cleanly (the caller then
/// DEFERS the whole draw so the pipeline's combined image-sampler is never left
/// un-written — a validation error).
pub fn derive_texture(
    user: &UserData,
    slot: &TextureSlot,
    mem: &(impl BoundedRead + ?Sized),
) -> Result<TextureBindingRange, VbufError> {
    let set_ptr = user
        .ptr_pair(slot.user_sgpr)
        .ok_or(VbufError::SlotOutOfRange(slot.user_sgpr))?;
    if set_ptr == 0 {
        return Err(VbufError::NullPointer);
    }
    let t_addr = set_ptr
        .checked_add(slot.t_offset)
        .ok_or(VbufError::NullPointer)?;
    let s_addr = set_ptr
        .checked_add(slot.s_offset)
        .ok_or(VbufError::NullPointer)?;

    let t_bytes = mem
        .read_ranged(t_addr, T_SHARP_SIZE)
        .map_err(|_| VbufError::MemoryFault)?;
    let s_bytes = mem
        .read_ranged(s_addr, S_SHARP_SIZE)
        .map_err(|_| VbufError::MemoryFault)?;
    let mut t_words = [0u32; 8];
    for (i, w) in t_words.iter_mut().enumerate() {
        *w = read_le_u32(&t_bytes, i * 4);
    }
    let s_words = [
        read_le_u32(&s_bytes, 0),
        read_le_u32(&s_bytes, 4),
        read_le_u32(&s_bytes, 8),
        read_le_u32(&s_bytes, 12),
    ];
    let texture = decode_t_sharp(t_words);
    if texture.is_null() {
        return Err(VbufError::NullDescriptor);
    }
    let sampler = decode_s_sharp(s_words);
    Ok(TextureBindingRange { texture, sampler })
}

/// Resolve one buffer slot to a [`BufferRange`], or the reason it defers.
fn resolve_slot(
    user: &UserData,
    slot: &BufferSlot,
    mem: &(impl BoundedRead + ?Sized),
) -> Result<BufferRange, VbufError> {
    // The pointer to the descriptor set lives in the user-SGPR pair.
    let set_ptr = user
        .ptr_pair(slot.user_sgpr)
        .ok_or(VbufError::SlotOutOfRange(slot.user_sgpr))?;
    if set_ptr == 0 {
        return Err(VbufError::NullPointer);
    }
    let desc_addr = set_ptr
        .checked_add(slot.desc_offset)
        .ok_or(VbufError::NullPointer)?;

    // Read the 16-byte V# through the range-validated seam — the pointer is untrusted.
    let bytes = mem
        .read_ranged(desc_addr, V_SHARP_SIZE)
        .map_err(|_| VbufError::MemoryFault)?;
    // read_ranged returns exactly V_SHARP_SIZE bytes on Ok, so the four reads are in range.
    let words = [
        read_le_u32(&bytes, 0),
        read_le_u32(&bytes, 4),
        read_le_u32(&bytes, 8),
        read_le_u32(&bytes, 12),
    ];
    let desc = decode_v_sharp(words);
    if desc.is_null() {
        return Err(VbufError::NullDescriptor);
    }
    Ok(BufferRange {
        addr: desc.base,
        size: desc.byte_span(),
        layout: slot.layout,
        desc,
    })
}

/// Read a little-endian `u32` at byte offset `at`. The caller sized the slice to exactly
/// [`V_SHARP_SIZE`], so the four indices are always in range.
fn read_le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat backing-buffer bounded reader for tests: guest address == index into a
    /// single immutable `Vec<u8>` based at `base`. Any read wholly inside `[base,
    /// base+len)` succeeds; anything else is the clean guest fault the decoder must
    /// survive. Mirrors the `.sb` parser's test seam — the minimal [`BoundedRead`] with
    /// no `VirtualMemoryManager` boilerplate.
    struct BufMem {
        base: u64,
        buf: Vec<u8>,
    }

    impl BufMem {
        fn new(base: u64, buf: Vec<u8>) -> Self {
            BufMem { base, buf }
        }
    }

    impl BoundedRead for BufMem {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            let start = addr
                .checked_sub(self.base)
                .ok_or("segfault: below region")? as usize;
            let end = start.checked_add(size).ok_or("segfault: overflow")?;
            if end > self.buf.len() {
                return Err("segfault: past region");
            }
            Ok(self.buf[start..end].to_vec())
        }
    }

    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    /// Build a 128-bit V# from its fields (the encode side of [`decode_v_sharp`]).
    fn encode_v_sharp(
        base: u64,
        stride: u32,
        num_records: u32,
        dfmt: u8,
        nfmt: u8,
        dst_sel: [u8; 4],
    ) -> [u32; 4] {
        let w0 = (base & 0xFFFF_FFFF) as u32;
        let w1 = ((base >> 32) as u32 & 0xFFFF) | ((stride & 0x3FFF) << 16);
        let w2 = num_records;
        let w3 = (dst_sel[0] as u32 & 0x7)
            | ((dst_sel[1] as u32 & 0x7) << 3)
            | ((dst_sel[2] as u32 & 0x7) << 6)
            | ((dst_sel[3] as u32 & 0x7) << 9)
            | ((nfmt as u32 & 0x7) << 12)
            | ((dfmt as u32 & 0xF) << 15);
        [w0, w1, w2, w3]
    }

    #[test]
    fn decode_v_sharp_hardware_word3_layout() {
        // Independent of encode_v_sharp: a literal word3 laid out exactly as GFX7 hardware
        // stores it. Fields (LSB→MSB): dst_sel x=4[2:0] y=5[5:3] z=6[8:6] w=7[11:9],
        // nfmt=7(float)[14:12], dfmt=14(_32_32_32_32)[18:15]. Packed:
        //   4 | 5<<3 | 6<<6 | 7<<9 | 7<<12 | 14<<15 = 0x0007_7FAC
        let w3 = 0x0007_7FAC;
        let d = decode_v_sharp([0, 0, 0, w3]);
        assert_eq!(
            d.dfmt,
            DataFormat::Format32_32_32_32,
            "dfmt from real word3"
        );
        assert_eq!(d.nfmt, NumFormat::Float, "nfmt from real word3");
        assert_eq!(d.dst_sel, [4, 5, 6, 7], "dst_sel from real word3");
    }

    // ---- AC #1: hand-built user-data block + V# → correct fields --------------

    #[test]
    fn decode_v_sharp_base_stride_records_format() {
        // AC #1: a vec4-float vertex-buffer V# (dfmt 14 = _32_32_32_32, nfmt 7 = float,
        // identity swizzle) decodes to the exact base/stride/records/format.
        let words = encode_v_sharp(0x0000_1234_5678, 16, 3, 14, 7, [1, 2, 3, 4]);
        let d = decode_v_sharp(words);
        assert_eq!(d.base, 0x0000_1234_5678);
        assert_eq!(d.stride, 16);
        assert_eq!(d.num_records, 3);
        assert_eq!(d.dfmt, DataFormat::Format32_32_32_32);
        assert_eq!(d.nfmt, NumFormat::Float);
        assert_eq!(d.dst_sel, [1, 2, 3, 4]);
        assert!(!d.is_null());
        // Strided span = stride * records.
        assert_eq!(d.byte_span(), 16 * 3);
    }

    #[test]
    fn dfmt_nfmt_table_for_corpus_formats() {
        // AC #1: the dfmt/nfmt table maps every corpus format to the right typed value.
        for (bits, want) in [
            (0u8, DataFormat::Invalid),
            (1, DataFormat::Format8),
            (2, DataFormat::Format16),
            (3, DataFormat::Format8_8),
            (4, DataFormat::Format32),
            (5, DataFormat::Format16_16),
            (10, DataFormat::Format8_8_8_8),
            (11, DataFormat::Format32_32),
            (12, DataFormat::Format16_16_16_16),
            (13, DataFormat::Format32_32_32),
            (14, DataFormat::Format32_32_32_32),
        ] {
            assert_eq!(DataFormat::from_bits(bits), want, "dfmt {bits}");
        }
        assert_eq!(DataFormat::from_bits(9), DataFormat::Other(9));
        for (bits, want) in [
            (0u8, NumFormat::Unorm),
            (1, NumFormat::Snorm),
            (4, NumFormat::Uint),
            (5, NumFormat::Sint),
            (7, NumFormat::Float),
        ] {
            assert_eq!(NumFormat::from_bits(bits), want, "nfmt {bits}");
        }
        assert_eq!(NumFormat::from_bits(3), NumFormat::Other(3));
        // Component counts for the modeled formats.
        assert_eq!(DataFormat::Format32.components(), Some(1));
        assert_eq!(DataFormat::Format32_32.components(), Some(2));
        assert_eq!(DataFormat::Format32_32_32.components(), Some(3));
        assert_eq!(DataFormat::Format32_32_32_32.components(), Some(4));
        assert_eq!(DataFormat::Invalid.components(), None);
    }

    #[test]
    fn byte_span_uses_element_size_when_stride_zero() {
        // A tightly-packed (stride 0) vec4-float buffer: span = 16 bytes * records.
        let words = encode_v_sharp(0x2000, 0, 4, 14, 7, [1, 2, 3, 4]);
        let d = decode_v_sharp(words);
        assert_eq!(d.stride, 0);
        assert_eq!(d.byte_span(), 16 * 4);
    }

    #[test]
    fn user_data_reads_pointer_pair_from_sh_regs() {
        // AC #1: the SPI_SHADER_USER_DATA_VS block reads back a preloaded pointer pair.
        let mut s = GpuState::default();
        // s[2:3] = descriptor-set pointer 0x1_0000 (corpus ABI).
        s.sh_regs
            .set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 2, 0x0001_0000);
        s.sh_regs.set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 3, 0x0000);
        let user = UserData::from_regs(&s, Stage::Vertex);
        assert_eq!(user.slot(2), Some(0x0001_0000));
        assert_eq!(user.ptr_pair(2), Some(0x0001_0000));
        // High word contributes the top 32 bits.
        s.sh_regs
            .set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 3, 0x0000_00AB);
        let user = UserData::from_regs(&s, Stage::Vertex);
        assert_eq!(user.ptr_pair(2), Some(0x0000_00AB_0001_0000));
        // Pair running past the 16-slot block is None.
        assert_eq!(user.ptr_pair(15), None);
    }

    // ---- AC #2: draw-time derivation over a corpus-style draw -----------------

    /// Build mock memory holding two descriptor sets (a vertex-buffer V# and a
    /// const-buffer V#) plus a stage's user-SGPR block pointing at them, mirroring the
    /// corpus `s[2:3]`-is-the-descriptor-pointer ABI.
    fn corpus_draw() -> (GpuState, FetchLayout, BufMem) {
        const BASE: u64 = 0x10_0000;
        let vb_addr = 0x20_0000u64;
        let cb_addr = 0x30_0000u64;

        // Descriptor set: [ vertex-buffer V# (16B) ][ const-buffer V# (16B) ].
        let mut mem = Vec::new();
        for w in encode_v_sharp(vb_addr, 16, 3, 14, 7, [1, 2, 3, 4]) {
            push_u32(&mut mem, w);
        }
        for w in encode_v_sharp(cb_addr, 0, 64, 4, 7, [1, 2, 3, 4]) {
            push_u32(&mut mem, w);
        }
        let mem = BufMem::new(BASE, mem);

        // User SGPRs: s[2:3] = descriptor-set pointer (BASE).
        let mut s = GpuState::default();
        s.sh_regs.set(
            sh_reg::SPI_SHADER_USER_DATA_VS_0 + 2,
            (BASE & 0xFFFF_FFFF) as u32,
        );
        s.sh_regs
            .set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 3, (BASE >> 32) as u32);

        // Layout: the VB V# at set offset 0, the CB V# at offset 16, both via s[2:3].
        let layout = FetchLayout {
            buffers: vec![
                BufferSlot {
                    user_sgpr: 2,
                    desc_offset: 0,
                    layout: ResLayout::VertexBuf,
                },
                BufferSlot {
                    user_sgpr: 2,
                    desc_offset: V_SHARP_SIZE as u64,
                    layout: ResLayout::ConstBuf,
                },
            ],
        };
        (s, layout, mem)
    }

    #[test]
    fn draw_derivation_returns_all_referenced_ranges() {
        // AC #2: a corpus-style draw derives the full set of referenced buffer ranges.
        let (s, layout, mem) = corpus_draw();
        let user = UserData::from_regs(&s, Stage::Vertex);
        let db = derive_buffer_ranges(&user, &layout, &mem);

        assert_eq!(db.deferred, vec![], "no binding should defer");
        assert_eq!(db.ranges.len(), 2);
        // Vertex buffer: base 0x20_0000, span stride*records = 16*3 = 48.
        assert_eq!(
            db.ranges[0].key_triple(),
            (0x20_0000, 48, ResLayout::VertexBuf)
        );
        // Const buffer: base 0x30_0000, stride 0 dfmt 32 → element 4B * 64 = 256.
        assert_eq!(
            db.ranges[1].key_triple(),
            (0x30_0000, 256, ResLayout::ConstBuf)
        );
        // Vertex-input part covers only the vertex-buffer binding.
        assert_eq!(db.vertex_input.attributes.len(), 1);
        assert_eq!(db.vertex_input.attributes[0].stride, 16);
        assert_eq!(
            db.vertex_input.attributes[0].dfmt,
            DataFormat::Format32_32_32_32
        );
        assert_eq!(db.vertex_input.attributes[0].nfmt, NumFormat::Float);
    }

    // ---- AC #3: malformed / null descriptors → clean per-draw defer -----------

    #[test]
    fn null_pointer_defers_no_crash() {
        // AC #3: a user-SGPR pair that is null (unprogrammed) → clean NullPointer defer.
        let s = GpuState::default(); // nothing programmed → all-zero user data
        let user = UserData::from_regs(&s, Stage::Vertex);
        let layout = FetchLayout {
            buffers: vec![BufferSlot {
                user_sgpr: 2,
                desc_offset: 0,
                layout: ResLayout::VertexBuf,
            }],
        };
        let mem = BufMem::new(0, Vec::new());
        let db = derive_buffer_ranges(&user, &layout, &mem);
        assert!(db.ranges.is_empty());
        assert_eq!(db.deferred, vec![(0, VbufError::NullPointer)]);
    }

    #[test]
    fn unmapped_descriptor_pointer_faults_cleanly() {
        // AC #3: a pointer to unmapped memory → clean MemoryFault (never an over-read).
        let mut s = GpuState::default();
        s.sh_regs
            .set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 2, 0xDEAD_0000);
        s.sh_regs.set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 3, 0);
        let user = UserData::from_regs(&s, Stage::Vertex);
        let layout = FetchLayout {
            buffers: vec![BufferSlot {
                user_sgpr: 2,
                desc_offset: 0,
                layout: ResLayout::VertexBuf,
            }],
        };
        // Backing buffer is elsewhere, so 0xDEAD_0000 is unmapped.
        let mem = BufMem::new(0x10_0000, vec![0u8; 64]);
        let db = derive_buffer_ranges(&user, &layout, &mem);
        assert!(db.ranges.is_empty());
        assert_eq!(db.deferred, vec![(0, VbufError::MemoryFault)]);
    }

    #[test]
    fn null_descriptor_defers_no_crash() {
        // AC #3: the pointer is valid but the V# it names is null (base 0, dfmt invalid).
        const BASE: u64 = 0x10_0000;
        let mut mem = Vec::new();
        // An all-zero V# → base 0, dfmt Invalid → is_null().
        for w in encode_v_sharp(0, 0, 0, 0, 0, [0; 4]) {
            push_u32(&mut mem, w);
        }
        let mem = BufMem::new(BASE, mem);
        let mut s = GpuState::default();
        s.sh_regs.set(
            sh_reg::SPI_SHADER_USER_DATA_VS_0 + 2,
            (BASE & 0xFFFF_FFFF) as u32,
        );
        s.sh_regs
            .set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 3, (BASE >> 32) as u32);
        let user = UserData::from_regs(&s, Stage::Vertex);
        let layout = FetchLayout {
            buffers: vec![BufferSlot {
                user_sgpr: 2,
                desc_offset: 0,
                layout: ResLayout::VertexBuf,
            }],
        };
        let db = derive_buffer_ranges(&user, &layout, &mem);
        assert!(db.ranges.is_empty());
        assert_eq!(db.deferred, vec![(0, VbufError::NullDescriptor)]);
    }

    #[test]
    fn one_bad_binding_does_not_drop_the_good_ones() {
        // AC #3: a draw with a good VB binding and a null CB binding keeps the good range
        // and defers only the bad one — a partial-but-clean derivation, no crash.
        const BASE: u64 = 0x10_0000;
        let vb_addr = 0x20_0000u64;
        let mut mem = Vec::new();
        for w in encode_v_sharp(vb_addr, 16, 3, 14, 7, [1, 2, 3, 4]) {
            push_u32(&mut mem, w);
        }
        // Second descriptor is all-zero (null).
        for w in encode_v_sharp(0, 0, 0, 0, 0, [0; 4]) {
            push_u32(&mut mem, w);
        }
        let mem = BufMem::new(BASE, mem);
        let mut s = GpuState::default();
        s.sh_regs.set(
            sh_reg::SPI_SHADER_USER_DATA_VS_0 + 2,
            (BASE & 0xFFFF_FFFF) as u32,
        );
        s.sh_regs
            .set(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 3, (BASE >> 32) as u32);
        let user = UserData::from_regs(&s, Stage::Vertex);
        let layout = FetchLayout {
            buffers: vec![
                BufferSlot {
                    user_sgpr: 2,
                    desc_offset: 0,
                    layout: ResLayout::VertexBuf,
                },
                BufferSlot {
                    user_sgpr: 2,
                    desc_offset: V_SHARP_SIZE as u64,
                    layout: ResLayout::ConstBuf,
                },
            ],
        };
        let db = derive_buffer_ranges(&user, &layout, &mem);
        assert_eq!(db.ranges.len(), 1);
        assert_eq!(db.ranges[0].layout, ResLayout::VertexBuf);
        assert_eq!(db.deferred, vec![(1, VbufError::NullDescriptor)]);
    }

    #[test]
    fn slot_out_of_range_defers() {
        // AC #3: a layout naming a user-SGPR pair past the 16-slot block defers cleanly.
        let s = GpuState::default();
        let user = UserData::from_regs(&s, Stage::Vertex);
        let layout = FetchLayout {
            buffers: vec![BufferSlot {
                user_sgpr: 15, // pair [15,16] runs past the block
                desc_offset: 0,
                layout: ResLayout::VertexBuf,
            }],
        };
        let mem = BufMem::new(0, Vec::new());
        let db = derive_buffer_ranges(&user, &layout, &mem);
        assert_eq!(db.deferred, vec![(0, VbufError::SlotOutOfRange(15))]);
    }

    #[test]
    fn saturating_span_never_wraps() {
        // A hostile stride * num_records must saturate, not wrap to a small span that
        // would let a fetch escape its intended range.
        let words = encode_v_sharp(0x1000, 0x3FFF, u32::MAX, 14, 7, [1, 2, 3, 4]);
        let d = decode_v_sharp(words);
        let span = d.byte_span();
        assert_eq!(span, u64::from(0x3FFFu32) * u64::from(u32::MAX));
        assert!(span > u64::from(u32::MAX));
    }

    // ---- T# / S# decode (doc-4 §C3/§C4) ------------------------------------

    #[test]
    fn decode_t_sharp_hardware_layout() {
        // A HAND-LAID hardware T#, not built by an encoder under test. Fields:
        //   word0 = 0x0000_0210 → base = 0x210 << 8 = 0x21000
        //   word1: dfmt=10 at [25:20] (10<<20 = 0x00A0_0000), nfmt=0 → 0x00A0_0000
        //   word2: width-1=63 [13:0], height-1=31 [27:14] → 63 | (31<<14) = 0x0007_C03F
        //   word3: tiling index 0 (linear)
        let words = [
            0x0000_0210,
            0x00A0_0000,
            0x0007_C03F,
            0x0000_0000,
            0,
            0,
            0,
            0,
        ];
        let t = decode_t_sharp(words);
        assert_eq!(t.base, 0x21000, "base = word0 << 8");
        assert_eq!(t.width, 64, "width-1=63 → 64");
        assert_eq!(t.height, 32, "height-1=31 → 32");
        assert_eq!(t.dfmt, 10, "dfmt at word1[25:20]");
        assert_eq!(t.nfmt, 0, "nfmt at word1[29:26]");
        assert_eq!(t.tiling_index, 0, "linear");
        assert!(!t.is_null());
        // Linear span = 64*32*4 = 8192.
        assert_eq!(t.byte_span(), 64 * 32 * 4);
    }

    #[test]
    fn decode_t_sharp_tiled_and_null() {
        // Tiling index 3 (word3[22:20] = 3<<20 = 0x0030_0000); non-linear byte span rounds
        // each extent up to whole 8×8 micro-tiles. A 5×3 texture → tiles 1×1 → 64 texels.
        let words = [0x0000_0100, 0, 4 | (2 << 14), 0x0030_0000, 0, 0, 0, 0];
        let t = decode_t_sharp(words);
        assert_eq!(t.tiling_index, 3);
        assert_eq!(t.width, 5);
        assert_eq!(t.height, 3);
        // 1×1 tiles × 64 texels/tile × 4 bytes.
        assert_eq!(t.byte_span(), 64 * 4, "tiled span rounds to whole tiles");

        // A zero-base / zero-extent T# is null → the draw must defer.
        let null = decode_t_sharp([0; 8]);
        assert!(null.is_null(), "zero base/extent → null T#");
    }

    #[test]
    fn decode_s_sharp_filter_bit() {
        // Hand-laid S#: word2[20] selects the filter. Point when clear, bilinear when set.
        let point = decode_s_sharp([0, 0, 0, 0]);
        assert!(!point.bilinear, "filter bit clear → point");
        let bilinear = decode_s_sharp([0, 0, 1 << 20, 0]);
        assert!(bilinear.bilinear, "filter bit set → bilinear");
    }

    #[test]
    fn derive_texture_reads_t_and_s_through_bounded_seam() {
        // Build a descriptor set in guest memory: T# (32 bytes) at offset 0, S# (16 bytes)
        // at offset 32, pointed at by user-SGPR pair s[0:1]. Hand-laid words.
        const SET: u64 = 0x4000;
        let mut data = Vec::new();
        // T# (8 words): base 0x5000>>8=0x50 in word0, width-1=1/height-1=1 in word2.
        push_u32(&mut data, 0x50); // base = 0x5000
        push_u32(&mut data, 10 << 20); // dfmt=10
        push_u32(&mut data, 1 | (1 << 14)); // 2×2
        push_u32(&mut data, 0); // linear
        for _ in 4..8 {
            push_u32(&mut data, 0);
        }
        // S# (4 words): filter bit set (bilinear).
        push_u32(&mut data, 0);
        push_u32(&mut data, 0);
        push_u32(&mut data, 1 << 20);
        push_u32(&mut data, 0);
        let mem = BufMem::new(SET, data);

        // user-SGPRs: s0/s1 = SET pointer (low, high).
        let mut words = [0u32; sh_reg::USER_DATA_SLOTS as usize];
        words[0] = (SET & 0xFFFF_FFFF) as u32;
        words[1] = (SET >> 32) as u32;
        let user = UserData { words };

        let range = derive_texture(&user, &CORPUS_TEXTURE_SLOT, &mem).expect("resolves");
        assert_eq!(range.texture.base, 0x5000);
        assert_eq!((range.texture.width, range.texture.height), (2, 2));
        assert!(range.sampler.bilinear, "S# filter bit → bilinear");
    }

    #[test]
    fn derive_texture_null_pointer_and_fault_defer() {
        // Unprogrammed user-SGPRs → null pointer → clean defer.
        let user = UserData {
            words: [0u32; sh_reg::USER_DATA_SLOTS as usize],
        };
        let mem = BufMem::new(0, Vec::new());
        assert_eq!(
            derive_texture(&user, &CORPUS_TEXTURE_SLOT, &mem),
            Err(VbufError::NullPointer),
        );

        // A pointer into unmapped memory faults through the bounded seam — never an
        // over-read.
        let mut words = [0u32; sh_reg::USER_DATA_SLOTS as usize];
        words[0] = 0xDEAD_0000;
        let user = UserData { words };
        assert_eq!(
            derive_texture(&user, &CORPUS_TEXTURE_SLOT, &mem),
            Err(VbufError::MemoryFault),
        );
    }
}
