//! `.sb` (OrbShdr) shader-binary container parser (doc-2 §1 `sb.rs`; doc-1 §3.3).
//!
//! Parses the OpenOrbis/Sony `"OrbShdr"` container that wraps precompiled GCN ISA
//! machine code with a [`ShaderBinaryInfo`] header + semantic-metadata tables. This
//! is the phase-4 entry point: it locates the header, validates it, and
//! returns a typed [`SbShader`] describing the *stage*, the *GCN code byte range*,
//! and the *semantic tables* — **without decoding a single GCN instruction** (that
//! is deferred to a separate phase-4 decoder) and **without touching the `ShaderProvider` chain** (also deferred).
//!
//! # Layout (verified against real Celeste `OrbShdr` blobs — game `.mgfxo` shaders and
//! captured GPU buffers at the shader program addresses; pinned by the
//! `shader_binary_info_matches_real_orbshdr` test)
//!
//! The container is `[ GCN machine code ][ ShaderBinaryInfo header ]` — the header
//! sits **immediately after** the code. `ShaderBinaryInfo` is a packed **28-byte**
//! little-endian struct:
//!
//! | Off  | Size | Field                          |
//! |------|------|--------------------------------|
//! | 0x00 | 7    | `m_signature` = `"OrbShdr"`    |
//! | 0x07 | 1    | `m_version`                    |
//! | 0x08 | 4    | bitfield word (see below)      |
//! | 0x0C | 1    | `m_chunkUsageBaseOffsetInDW`   |
//! | 0x0D | 1    | `m_numInputUsageSlots`         |
//! | 0x0E | 1    | flags: `m_isSrt:1`, `m_isSrtUsedInfoValid:1`, `m_isExtendedUsageInfo:1`, rsv:5 |
//! | 0x0F | 1    | `m_reserved3`                  |
//! | 0x10 | 4    | `m_shaderHash0`                |
//! | 0x14 | 4    | `m_shaderHash1`                |
//! | 0x18 | 4    | `m_crc32`                      |
//!
//! The 32-bit word at `0x08` packs LSB-first: `m_pssl_or_cg:1`, `m_cached:1`,
//! `m_type:4` (stage), `m_source_type:2`, `m_length:24` (GCN code size in bytes,
//! occupying the high 24 bits, i.e. bytes `0x09..=0x0B`).
//!
//! # Semantic tables (doc-1 §3.3)
//!
//! `VertexInputSemantic` / `VertexExportSemantic` / `PixelInputSemantic` /
//! `PixelSemanticMapping` are **not** stored inside `ShaderBinaryInfo` — they live in
//! the gnmx register-setup structs (`GnmVsShader` / `GnmPsShader`) the game builds in
//! guest memory, with element counts carried by that block's `m_num*Semantics` byte
//! fields. [`parse_sb`] parses only the `.sb` container; [`parse_vs_semantics`] /
//! [`parse_ps_semantics`] parse a caller-supplied gnmx semantic array. The wiring
//! that hands those blocks over lives in `state.rs`/`exec.rs`, so
//! [`SbShader::semantics`] is [`Semantics::default`] (empty) until a register block
//! is supplied; the table parsers are the reference the draw path builds on.
//!
//! # `.sb` address derivation (`SPI_SHADER_PGM_LO/HI` → code start; P4-09)
//!
//! A guest sets the shader program address in two 32-bit registers,
//! `SPI_SHADER_PGM_LO_*` and `SPI_SHADER_PGM_HI_*`. They hold the **start of the GCN
//! machine code** (not the header) as a 256-byte-aligned address, shifted right by 8:
//!
//! ```text
//! code_start = ((PGM_HI as u64) << 32 | PGM_LO as u64) << 8
//! ```
//!
//! The `ShaderBinaryInfo` footer sits shortly *after* the code — at `code_start + m_length`
//! plus a small input-usage-slot/hash table (8..64 bytes observed on retail shaders; the code
//! itself ends in `s_endpgm`). Because `m_length` is only known once the footer is read
//! (chicken-and-egg), [`parse_sb`] locates it by scanning forward from `code_start` for the
//! `"OrbShdr"` magic and validating `code_start + m_length <= header_addr <= code_start +
//! m_length + MAX_FOOTER_GAP` (see [`validate_sb_candidate`]). The draw path feeds `code_start`
//! (via [`pgm_addr`]) into [`parse_sb`].
//!
//! # Hard constraints
//!
//! - **Never decrypts.** Encrypted/garbage input has no plaintext `"OrbShdr"` magic
//!   in range and is rejected with a clean [`SbParseError`] — there is no crypto here.
//! - **No GCN decode**, **no `ShaderProvider` coupling**, **Vulkan-free** (ps4-core
//!   only).

use ps4_core::bounded_read::BoundedRead;

use crate::shader::source::Stage;

/// The 7-byte `ShaderBinaryInfo::m_signature` magic (doc-1 §3.3).
pub const ORBSHDR_MAGIC: &[u8; 7] = b"OrbShdr";

/// Size in bytes of the packed `ShaderBinaryInfo` header (doc-1 §3.3).
pub const SHADER_BINARY_INFO_SIZE: usize = 28;

/// How far forward from `code_start` [`parse_sb`] will scan for the `"OrbShdr"` magic
/// before giving up. A GCN shader body is far smaller than this; the bound keeps a
/// garbage/encrypted address from walking arbitrarily far into guest memory.
const MAX_SCAN_BYTES: u64 = 1 << 20; // 1 MiB

/// GCN shader stage decoded from `ShaderBinaryInfo::m_type` (the 4-bit field at word
/// `0x08`, bits [5:2]). `m_type = 1` (Vertex) and `m_type = 0` (Pixel) are corroborated by
/// real Celeste blobs: the capture buffers at the `SPI_SHADER_PGM_LO_VS` program address
/// carry `m_type = 1`, and a game `.mgfxo` pixel shader carries `m_type = 0`. The extended
/// values (Export/Local/Compute/Geometry/Hull/Domain = 2/3/4/5/7/8) are not exercised by
/// Celeste and are our working model pending a broader shader corpus. The vertex/export/
/// local trio are all vertex-stage variants distinguished by where their output is routed
/// (HW-stage role, doc-2 §C8) — kept distinct rather than collapsed to the logical [`Stage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SbStage {
    /// `0` — pixel / fragment shader.
    Pixel,
    /// `1` — vertex shader (VS→VS: no geometry/tessellation).
    Vertex,
    /// `2` — export shader (VS acting as ES, feeds a geometry shader).
    Export,
    /// `3` — local shader (VS acting as LS, feeds a hull shader / tessellation).
    Local,
    /// `4` — compute shader.
    Compute,
    /// `5` — geometry shader.
    Geometry,
    /// `7` — hull shader.
    Hull,
    /// `8` — domain shader (DS running in the VS slot).
    Domain,
}

impl SbStage {
    /// Decode the 4-bit `m_type`. `6` (`kUnknown`) and any other value are rejected.
    fn from_m_type(m_type: u32) -> Result<Self, SbParseError> {
        Ok(match m_type {
            0 => SbStage::Pixel,
            1 => SbStage::Vertex,
            2 => SbStage::Export,
            3 => SbStage::Local,
            4 => SbStage::Compute,
            5 => SbStage::Geometry,
            7 => SbStage::Hull,
            8 => SbStage::Domain,
            other => return Err(SbParseError::UnknownStage(other as u8)),
        })
    }

    /// The logical [`Stage`] this maps to, if the shader path models one. The
    /// vertex-family HW roles (Vertex/Export/Local/Domain) all present as
    /// [`Stage::Vertex`]; Pixel maps to [`Stage::Pixel`]. Compute/Geometry/Hull have
    /// no logical [`Stage`] yet (that enum only grows as stages are supported).
    pub fn logical_stage(self) -> Option<Stage> {
        match self {
            SbStage::Vertex | SbStage::Export | SbStage::Local | SbStage::Domain => {
                Some(Stage::Vertex)
            }
            SbStage::Pixel => Some(Stage::Pixel),
            SbStage::Compute | SbStage::Geometry | SbStage::Hull => None,
        }
    }
}

/// The parsed `ShaderBinaryInfo` header fields (doc-1 §3.3). Raw, unvalidated beyond
/// magic + stage decode; the derived guest ranges live on [`SbShader`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShaderBinaryInfo {
    /// `m_version` byte.
    pub version: u8,
    /// PSSL vs Cg source flag (`m_pssl_or_cg`).
    pub pssl_or_cg: bool,
    /// `m_cached` flag.
    pub cached: bool,
    /// Decoded shader stage (`m_type`).
    pub stage: SbStage,
    /// `m_source_type` (2 bits).
    pub source_type: u8,
    /// `m_length` — GCN machine-code size in bytes (24 bits).
    pub code_len: u32,
    /// `m_chunkUsageBaseOffsetInDW` — DWORD offset back to the input-usage/chunk table.
    pub chunk_usage_base_offset_dw: u8,
    /// `m_numInputUsageSlots`.
    pub num_input_usage_slots: u8,
    /// `m_isSrt` — shader uses a shader resource table.
    pub is_srt: bool,
    /// `m_isSrtUsedInfoValid`.
    pub is_srt_used_info_valid: bool,
    /// `m_isExtendedUsageInfo`.
    pub is_extended_usage_info: bool,
    /// `m_shaderHash0`.
    pub shader_hash0: u32,
    /// `m_shaderHash1`.
    pub shader_hash1: u32,
    /// `m_crc32`.
    pub crc32: u32,
}

impl ShaderBinaryInfo {
    /// Parse a 28-byte `ShaderBinaryInfo` from a byte slice starting at the magic.
    /// Validates the magic and the stage; every other field is taken verbatim.
    fn parse(bytes: &[u8]) -> Result<Self, SbParseError> {
        if bytes.len() < SHADER_BINARY_INFO_SIZE {
            return Err(SbParseError::Truncated);
        }
        if &bytes[0..7] != ORBSHDR_MAGIC {
            return Err(SbParseError::BadMagic);
        }
        let version = bytes[7];
        let word = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let pssl_or_cg = (word & 0x1) != 0;
        let cached = ((word >> 1) & 0x1) != 0;
        let stage = SbStage::from_m_type((word >> 2) & 0xF)?;
        let source_type = ((word >> 6) & 0x3) as u8;
        let code_len = (word >> 8) & 0x00FF_FFFF;
        let chunk_usage_base_offset_dw = bytes[12];
        let num_input_usage_slots = bytes[13];
        let flags = bytes[14];
        // bytes[15] is m_reserved3.
        let shader_hash0 = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let shader_hash1 = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let crc32 = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        Ok(ShaderBinaryInfo {
            version,
            pssl_or_cg,
            cached,
            stage,
            source_type,
            code_len,
            chunk_usage_base_offset_dw,
            num_input_usage_slots,
            is_srt: (flags & 0x1) != 0,
            is_srt_used_info_valid: (flags & 0x2) != 0,
            is_extended_usage_info: (flags & 0x4) != 0,
            shader_hash0,
            shader_hash1,
            crc32,
        })
    }
}

/// A `VertexInputSemantic` entry (doc-1 §3.3): binds a vertex-fetch semantic to a
/// destination VGPR. 4 bytes, plain byte fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexInputSemantic {
    /// Semantic index the vertex fetch fills (`m_semantic`).
    pub semantic: u8,
    /// Destination VGPR (`m_vgpr`).
    pub vgpr: u8,
    /// Component count 1..=4 (`m_sizeInElements`).
    pub size_in_elements: u8,
}

impl VertexInputSemantic {
    const SIZE: usize = 4;
    fn parse(b: &[u8]) -> Self {
        VertexInputSemantic {
            semantic: b[0],
            vgpr: b[1],
            size_in_elements: b[2],
        }
    }
}

/// A `VertexExportSemantic` entry (doc-1 §3.3): a VS output param slot. 2 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexExportSemantic {
    /// Semantic index (`m_semantic`).
    pub semantic: u8,
    /// Export/param slot 0..=31 (`m_outIndex`, 5 bits).
    pub out_index: u8,
    /// Half-float export mode (`m_exportF16`, 2 bits).
    pub export_f16: u8,
}

impl VertexExportSemantic {
    const SIZE: usize = 2;
    fn parse(b: &[u8]) -> Self {
        // byte1: m_outIndex:5, m_reserved:1, m_exportF16:2 (LSB-first).
        VertexExportSemantic {
            semantic: b[0],
            out_index: b[1] & 0x1F,
            export_f16: (b[1] >> 6) & 0x3,
        }
    }
}

/// A `PixelInputSemantic` entry (doc-1 §3.3): a PS varying-input interpolation mode.
/// 2 bytes (a single little-endian `u16` bitfield).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelInputSemantic {
    /// Semantic index (`m_semantic`, 8 bits).
    pub semantic: u8,
    /// Default-value selector (`m_defaultValue`, 2 bits).
    pub default_value: u8,
    /// Flat-shaded (no interpolation) flag (`m_isFlatShaded`).
    pub is_flat_shaded: bool,
    /// Linear (non-perspective) interpolation flag (`m_isLinear`).
    pub is_linear: bool,
    /// Custom-interpolation flag (`m_isCustom`).
    pub is_custom: bool,
}

impl PixelInputSemantic {
    const SIZE: usize = 2;
    fn parse(b: &[u8]) -> Self {
        let w = u16::from_le_bytes([b[0], b[1]]);
        PixelInputSemantic {
            semantic: (w & 0xFF) as u8,
            default_value: ((w >> 8) & 0x3) as u8,
            is_flat_shaded: ((w >> 10) & 0x1) != 0,
            is_linear: ((w >> 11) & 0x1) != 0,
            is_custom: ((w >> 12) & 0x1) != 0,
        }
    }
}

/// A `PixelSemanticMapping` entry (doc-1 §3.3): links a VS-output param slot to a
/// PS-input semantic slot. Stored as one little-endian `u32` in the gnmx block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelSemanticMapping {
    /// VS-output / export slot feeding this PS input.
    pub out_index: u8,
    /// PS-input semantic slot it drives.
    pub in_index: u8,
}

impl PixelSemanticMapping {
    const SIZE: usize = 4;
    fn parse(b: &[u8]) -> Self {
        // Low byte = VS export slot, next byte = PS input slot (upper bytes reserved).
        PixelSemanticMapping {
            out_index: b[0],
            in_index: b[1],
        }
    }
}

/// The semantic-metadata tables that drive vertex-attribute fetch and VS→PS varying
/// linkage (doc-1 §3.3, doc-2 §C4). Sourced from the gnmx register-setup block, not
/// the `.sb` header, so [`parse_sb`] leaves this [`Semantics::default`] (empty) until
/// [`parse_vs_semantics`] / [`parse_ps_semantics`] populate it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Semantics {
    /// `VertexInputSemantic` table (VS attribute fetch).
    pub vertex_inputs: Vec<VertexInputSemantic>,
    /// `VertexExportSemantic` table (VS output params).
    pub vertex_exports: Vec<VertexExportSemantic>,
    /// `PixelInputSemantic` table (PS varying interpolation modes).
    pub pixel_inputs: Vec<PixelInputSemantic>,
    /// `PixelSemanticMapping` table (VS-output → PS-input linkage).
    pub pixel_mappings: Vec<PixelSemanticMapping>,
}

/// A parsed `.sb` (OrbShdr) shader binary: the stage, the guest byte range of the raw
/// GCN machine code, the header fields, and any semantic tables (empty unless a gnmx
/// register block was supplied). Carries no decoded instructions — a phase-4 decoder consumes
/// [`SbShader::code_range`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SbShader {
    /// Decoded GCN stage (HW-stage role; see [`SbShader::logical_stage`]).
    pub stage: SbStage,
    /// Guest `[start, end)` byte range of the raw GCN machine code (`end - start ==
    /// header.code_len`). `end` is `code_start + m_length` (the `s_endpgm` past-the-end); the
    /// `ShaderBinaryInfo` footer sits a small input-usage-table gap *after* `end`, not at it.
    pub code_range: std::ops::Range<u64>,
    /// The parsed `ShaderBinaryInfo` header.
    pub info: ShaderBinaryInfo,
    /// Semantic tables (empty unless populated from a gnmx register block).
    pub semantics: Semantics,
}

impl SbShader {
    /// The logical [`Stage`] this shader presents as, if modeled (see
    /// [`SbStage::logical_stage`]).
    pub fn logical_stage(&self) -> Option<Stage> {
        self.stage.logical_stage()
    }
}

/// Why a `.sb` parse failed. Every variant is a clean rejection — malformed,
/// truncated, or non-plaintext (e.g. encrypted) input never panics or reads OOB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SbParseError {
    /// The `"OrbShdr"` magic was not found within [`MAX_SCAN_BYTES`] of `code_start`
    /// (garbage, encrypted, or wrong address — never decrypted).
    MagicNotFound,
    /// A candidate header did not start with the `"OrbShdr"` magic.
    BadMagic,
    /// The header (or a semantic table) ran past the readable buffer / guest mapping.
    Truncated,
    /// `code_start + m_length != header_addr`: the located header does not describe
    /// the code region it was reached from (corrupt container).
    LengthMismatch,
    /// `m_type` was `6` (`kUnknown`) or an undefined value.
    UnknownStage(u8),
    /// A guest read failed (address not backed by the memory manager).
    MemoryFault,
}

impl std::fmt::Display for SbParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SbParseError::MagicNotFound => write!(f, "OrbShdr magic not found in scan range"),
            SbParseError::BadMagic => write!(f, "candidate header lacks OrbShdr magic"),
            SbParseError::Truncated => write!(f, "shader binary truncated"),
            SbParseError::LengthMismatch => write!(f, "m_length does not match header offset"),
            SbParseError::UnknownStage(t) => write!(f, "unknown/unsupported shader stage {t}"),
            SbParseError::MemoryFault => write!(f, "guest memory read fault"),
        }
    }
}

impl std::error::Error for SbParseError {}

/// Combine `SPI_SHADER_PGM_LO`/`PGM_HI` into the GCN **code start** address (doc-1
/// §3.3, P4-09): `((hi << 32) | lo) << 8`. The registers hold the 256-byte-aligned
/// code address shifted right by 8. The draw path uses this to feed [`parse_sb`].
///
/// ```
/// # use ps4_gnm::shader::sb::pgm_addr;
/// // A game programs PGM_LO/HI for GCN code at guest 0x0020_0000.
/// // 0x0020_0000 >> 8 == 0x2000, split across the two registers.
/// assert_eq!(pgm_addr(0x0000_2000, 0x0000_0000), 0x0020_0000);
/// ```
#[inline]
pub fn pgm_addr(pgm_lo: u32, pgm_hi: u32) -> u64 {
    (((pgm_hi as u64) << 32) | pgm_lo as u64) << 8
}

/// Read exactly `len` bytes at guest `addr` through the range-validated seam, mapping a
/// read fault to a clean error. The reader is a [`BoundedRead`], so an `addr`/`len` that
/// straddles a mapping boundary is rejected instead of over-reading — this is the seam that
/// keeps a garbage/guest-controlled shader address from walking off its mapping.
fn read_exact(
    mem: &(impl BoundedRead + ?Sized),
    addr: u64,
    len: usize,
) -> Result<Vec<u8>, SbParseError> {
    mem.read_ranged(addr, len)
        .map_err(|_| SbParseError::MemoryFault)
}

/// Parse the `.sb` (OrbShdr) container whose GCN code begins at `code_start`.
///
/// `code_start` is the address from `SPI_SHADER_PGM_LO/HI` (see [`pgm_addr`]). The
/// `ShaderBinaryInfo` header sits at `code_start + m_length`, but `m_length` is only
/// known after the header is read, so this scans forward from `code_start` for the
/// `"OrbShdr"` magic (bounded by [`MAX_SCAN_BYTES`]), parses the header, and validates
/// `code_start + m_length == header_addr`. Encrypted/garbage input has no plaintext
/// magic in range and is rejected with [`SbParseError::MagicNotFound`] — never
/// decrypted.
///
/// Returns [`SbShader`] with empty [`Semantics`]; populate them from the gnmx block
/// via [`parse_vs_semantics`] / [`parse_ps_semantics`] (see module doc for the semantic-table wiring).
pub fn parse_sb(
    code_start: u64,
    mem: &(impl BoundedRead + ?Sized),
) -> Result<SbShader, SbParseError> {
    // A stray "OrbShdr" byte sequence inside the GCN code region is a false positive
    // (7 exact bytes — rare, but possible). Rather than abort the whole shader on the
    // first candidate that fails validation, resume the scan one byte past it and keep
    // looking until the real header is found or the scan window is exhausted (task-67).
    let mut scan_off: u64 = 0;
    let mut last_err: Option<SbParseError> = None;
    loop {
        let header_addr = match scan_for_magic(mem, code_start, scan_off) {
            Ok(a) => a,
            // No further magic in range: surface the last candidate's validation
            // failure if we hit one (a false "OrbShdr" that didn't validate), else
            // MagicNotFound.
            Err(SbParseError::MagicNotFound) => {
                return Err(last_err.unwrap_or(SbParseError::MagicNotFound));
            }
            Err(e) => return Err(e),
        };
        match validate_sb_candidate(code_start, header_addr, mem) {
            Ok(shader) => return Ok(shader),
            Err(e) => {
                // False positive — resume just past this magic.
                last_err = Some(e);
                scan_off = match header_addr
                    .checked_sub(code_start)
                    .and_then(|rel| rel.checked_add(1))
                {
                    Some(o) => o,
                    None => return Err(last_err.unwrap()),
                };
            }
        }
    }
}

/// The `OrbShdr` footer does **not** sit tight against the end of the GCN code: a small
/// input-usage-slot / hash table (`m_numInputUsageSlots` entries + chunk/hash words) is laid
/// down between `code_start + m_length` and the footer. Real Orbis shaders observed a gap of
/// 8..64 bytes (RE'd from a retail managed-runtime title's shader region — each shader is
/// 256-byte-aligned, its code ends in `s_endpgm`, then the table, then the footer). The bound
/// stays well under the 256-byte shader alignment so a stray `OrbShdr` deeper in the code is
/// still rejected as a false positive. Hand-built test/corpus blobs use a zero gap (header
/// immediately after code), which this range trivially admits.
const MAX_FOOTER_GAP: u64 = 256;

/// `s_endpgm` — the GCN wave-terminate instruction (little-endian dword `00 00 81 bf`). Every
/// real shader's code region ends in it; [`validate_sb_candidate`] requires the last code dword
/// to be `s_endpgm` so a stray `"OrbShdr"` inside the code (whose garbage `m_length` might place
/// `code_end` within [`MAX_FOOTER_GAP`] of the false magic) is rejected — the gap bound alone
/// admits it, the terminator check does not.
const S_ENDPGM: u32 = 0xBF81_0000;

/// Read + validate the `ShaderBinaryInfo` header at a candidate `header_addr` reached by
/// scanning from `code_start`. The footer must sit at `code_start + m_length` **plus a small
/// input-usage-table gap** ([`MAX_FOOTER_GAP`]); a footer *before* the code end, or improbably
/// far past it, means this "OrbShdr" was a false positive inside the code (see [`parse_sb`],
/// which resumes the scan on this error). As a second guard against a false magic whose garbage
/// `m_length` coincidentally lands within the gap, the last code dword must be [`S_ENDPGM`]. The
/// returned [`code_range`](SbShader::code_range) is exactly the `m_length` code bytes — it
/// excludes the trailing usage table so the decoder never walks past `s_endpgm` into the table.
fn validate_sb_candidate(
    code_start: u64,
    header_addr: u64,
    mem: &(impl BoundedRead + ?Sized),
) -> Result<SbShader, SbParseError> {
    let raw = read_exact(mem, header_addr, SHADER_BINARY_INFO_SIZE)?;
    let info = ShaderBinaryInfo::parse(&raw)?;

    let code_end = code_start
        .checked_add(info.code_len as u64)
        .ok_or(SbParseError::LengthMismatch)?;
    // Footer sits at code_end + gap (gap = input-usage/hash table). Reject a magic before the
    // code end (false positive inside the code) or improbably far past it.
    if header_addr < code_end || header_addr - code_end > MAX_FOOTER_GAP {
        return Err(SbParseError::LengthMismatch);
    }

    // The gap bound alone would admit a stray "OrbShdr" whose garbage m_length lands code_end
    // within MAX_FOOTER_GAP of the false magic. A real shader's code ends in s_endpgm, so require
    // the last code dword to be it — the discriminator between the true footer and a coincidence.
    // A code region too short to hold one dword can't be a shader.
    let last_dword = code_end
        .checked_sub(4)
        .filter(|&a| a >= code_start)
        .ok_or(SbParseError::LengthMismatch)?;
    let tail = read_exact(mem, last_dword, 4)?;
    if u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) != S_ENDPGM {
        return Err(SbParseError::LengthMismatch);
    }

    Ok(SbShader {
        stage: info.stage,
        code_range: code_start..code_end,
        info,
        semantics: Semantics::default(),
    })
}

/// Scan forward from `code_start` for the 7-byte `"OrbShdr"` magic, returning the
/// address of the first match. Reads in bounded windows and stops at the first
/// unreadable byte or [`MAX_SCAN_BYTES`], so a garbage/encrypted address can't walk
/// arbitrarily far. The window is *best-effort*: it shrinks on a guest fault so a
/// short mapping (or a small test blob) is still fully scanned, and a hit only
/// requires the 7 magic bytes to be present — the 28-byte header is bounds-checked by
/// [`parse_sb`]'s `read_exact`, which turns a header running past the mapping into a
/// clean [`SbParseError::Truncated`]/[`SbParseError::MemoryFault`].
fn scan_for_magic(
    mem: &(impl BoundedRead + ?Sized),
    code_start: u64,
    start_off: u64,
) -> Result<u64, SbParseError> {
    const CHUNK: usize = 4096;
    let magic_len = ORBSHDR_MAGIC.len();
    // Resume from `start_off` (bytes past `code_start`) so a caller can rescan past a
    // false-positive magic; the MAX_SCAN_BYTES bound stays measured from code_start.
    let mut off: u64 = start_off;
    while off < MAX_SCAN_BYTES {
        let addr = code_start
            .checked_add(off)
            .ok_or(SbParseError::MagicNotFound)?;
        // Read as large a window as the mapping allows: try CHUNK (+ a magic_len-1
        // overlap so a magic straddling the next window is still caught), shrinking
        // on fault down to the bare magic. If not even `magic_len` bytes are
        // readable, the scan has reached the end of the mapping — stop cleanly.
        let buf = match read_shrinking(mem, addr, CHUNK + magic_len - 1, magic_len) {
            Some(b) => b,
            None => return Err(SbParseError::MagicNotFound),
        };
        // Scan every position that can still hold the 7 magic bytes.
        let scanned = buf.len(); // bytes actually read this window (may be < CHUNK)
        let last = scanned - magic_len;
        for i in 0..=last {
            if &buf[i..i + magic_len] == ORBSHDR_MAGIC {
                return Ok(addr + i as u64);
            }
        }
        // Advance by the number of scanned positions (`scanned - (magic_len - 1)`),
        // keeping a magic_len-1 overlap so a magic straddling the boundary is caught.
        // Using the *actual* window size (not a fixed CHUNK) means a short read near
        // the end of a mapping still advances correctly instead of skipping bytes.
        let step = (scanned - (magic_len - 1)) as u64;
        off = off.checked_add(step).ok_or(SbParseError::MagicNotFound)?;
    }
    Err(SbParseError::MagicNotFound)
}

/// Read up to `want` bytes at `addr`, halving the request on a guest fault until it
/// succeeds or drops below `floor`. Returns `None` if even `floor` bytes are
/// unreadable (end of mapping / unmapped). Lets the magic scan work against both a
/// full-size guest mapping and a small backing buffer without over-reading.
fn read_shrinking(
    mem: &(impl BoundedRead + ?Sized),
    addr: u64,
    want: usize,
    floor: usize,
) -> Option<Vec<u8>> {
    let mut len = want;
    loop {
        if let Ok(b) = mem.read_ranged(addr, len) {
            return Some(b);
        }
        if len <= floor {
            return None;
        }
        len = (len / 2).max(floor);
    }
}

/// Parse a `VertexInputSemantic` + `VertexExportSemantic` pair of tables from a gnmx
/// register-setup block in guest memory (doc-1 §3.3, doc-2 §C4). The counts come from
/// the block's `m_num*Semantics` fields (supplied by the register-setup block caller); `input_addr`/
/// `export_addr` point at the respective inline arrays. Returns the two tables merged
/// into a [`Semantics`] (pixel tables empty). Bounds-safe: an out-of-range read is a
/// clean [`SbParseError::MemoryFault`].
pub fn parse_vs_semantics(
    mem: &(impl BoundedRead + ?Sized),
    input_addr: u64,
    num_inputs: usize,
    export_addr: u64,
    num_exports: usize,
) -> Result<Semantics, SbParseError> {
    let mut s = Semantics::default();
    let ins = read_exact(mem, input_addr, num_inputs * VertexInputSemantic::SIZE)?;
    for c in ins.chunks_exact(VertexInputSemantic::SIZE) {
        s.vertex_inputs.push(VertexInputSemantic::parse(c));
    }
    let exs = read_exact(mem, export_addr, num_exports * VertexExportSemantic::SIZE)?;
    for c in exs.chunks_exact(VertexExportSemantic::SIZE) {
        s.vertex_exports.push(VertexExportSemantic::parse(c));
    }
    Ok(s)
}

/// Parse a `PixelInputSemantic` + `PixelSemanticMapping` pair of tables from a gnmx
/// `GnmPsShader` register-setup block (doc-1 §3.3). Counts come from the block
/// Returns a [`Semantics`] with only the pixel tables populated. Bounds-
/// safe like [`parse_vs_semantics`].
pub fn parse_ps_semantics(
    mem: &(impl BoundedRead + ?Sized),
    input_addr: u64,
    num_inputs: usize,
    mapping_addr: u64,
    num_mappings: usize,
) -> Result<Semantics, SbParseError> {
    let mut s = Semantics::default();
    let ins = read_exact(mem, input_addr, num_inputs * PixelInputSemantic::SIZE)?;
    for c in ins.chunks_exact(PixelInputSemantic::SIZE) {
        s.pixel_inputs.push(PixelInputSemantic::parse(c));
    }
    let maps = read_exact(mem, mapping_addr, num_mappings * PixelSemanticMapping::SIZE)?;
    for c in maps.chunks_exact(PixelSemanticMapping::SIZE) {
        s.pixel_mappings.push(PixelSemanticMapping::parse(c));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat backing-buffer bounded reader for tests: guest address == index into a
    /// single immutable `Vec<u8>` based at `base`. `read_ranged` is bounds-checked against
    /// the buffer, so a read that would run past the end returns `Err` — exactly the guest
    /// fault the parser must survive. This is the minimal [`BoundedRead`] seam the parser
    /// takes; no `VirtualMemoryManager` boilerplate is needed for a read-only test.
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
        /// Bounds-checked read against the flat buffer: any read wholly inside
        /// `[base, base+len)` succeeds; anything else is a fault (the guest segfault
        /// the parser must turn into a clean `Err`).
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            let start = addr
                .checked_sub(self.base)
                .ok_or("Invalid memory address (segfault)")? as usize;
            let end = start
                .checked_add(size)
                .ok_or("Invalid memory address (segfault)")?;
            if end > self.buf.len() {
                return Err("Invalid memory address (segfault)");
            }
            Ok(self.buf[start..end].to_vec())
        }
    }

    /// Ground-truth witness: a real 28-byte `ShaderBinaryInfo` header, captured verbatim
    /// from a Celeste `.mgfxo` shader (`SpriteEffect.ps4.mgfxo`, first `OrbShdr` entry).
    /// The parser must recover exactly these fields — this pins the layout (magic, version,
    /// bitfield packing, byte offsets, hashes) to real console shader bytes.
    #[test]
    fn shader_binary_info_matches_real_orbshdr() {
        // OrbShdr | ver 07 | bitfield 41 78 00 00 | 02 02 0c 05 | hash0 | hash1 | crc32
        let header: [u8; SHADER_BINARY_INFO_SIZE] = [
            0x4f, 0x72, 0x62, 0x53, 0x68, 0x64, 0x72, // "OrbShdr"
            0x07, // m_version
            0x41, 0x78, 0x00, 0x00, // bitfield word = 0x00007841
            0x02, // m_chunkUsageBaseOffsetInDW
            0x02, // m_numInputUsageSlots
            0x0c, // flags (m_isExtendedUsageInfo set)
            0x05, // m_reserved3
            0xd3, 0x91, 0xb5, 0xfd, // m_shaderHash0
            0x00, 0x00, 0x00, 0x00, // m_shaderHash1
            0x9b, 0x36, 0xaf, 0x93, // m_crc32
        ];
        let info = ShaderBinaryInfo::parse(&header).expect("real OrbShdr header parses");
        assert_eq!(info.version, 7);
        assert!(info.pssl_or_cg); // bit0 of 0x7841
        assert!(!info.cached); // bit1
        assert_eq!(info.stage, SbStage::Pixel); // m_type = (0x7841>>2)&0xF = 0
        assert_eq!(info.source_type, 1); // (0x7841>>6)&0x3
        assert_eq!(info.code_len, 0x78); // 0x7841>>8 = 120 bytes of GCN code
        assert_eq!(info.chunk_usage_base_offset_dw, 0x02);
        assert_eq!(info.num_input_usage_slots, 0x02);
        assert!(info.is_extended_usage_info); // flags bit2
        assert_eq!(info.shader_hash0, 0xfdb5_91d3);
        assert_eq!(info.shader_hash1, 0x0000_0000);
        assert_eq!(info.crc32, 0x93af_369b);

        // Stage numbering corroborated by the capture: the blob at the VS program address
        // (SPI_SHADER_PGM_LO_VS) carries m_type = 1.
        assert_eq!(SbStage::from_m_type(1).unwrap(), SbStage::Vertex);
        assert_eq!(SbStage::from_m_type(0).unwrap(), SbStage::Pixel);
    }

    /// Encode a `ShaderBinaryInfo` bitfield word (offset 0x08).
    fn info_word(pssl_or_cg: bool, cached: bool, m_type: u32, src: u32, len: u32) -> u32 {
        (pssl_or_cg as u32)
            | ((cached as u32) << 1)
            | ((m_type & 0xF) << 2)
            | ((src & 0x3) << 6)
            | ((len & 0x00FF_FFFF) << 8)
    }

    /// Build a 28-byte `ShaderBinaryInfo` header.
    #[allow(clippy::too_many_arguments)]
    fn build_header(
        version: u8,
        m_type: u32,
        code_len: u32,
        flags: u8,
        hash0: u32,
        hash1: u32,
        crc32: u32,
    ) -> Vec<u8> {
        let mut h = Vec::with_capacity(SHADER_BINARY_INFO_SIZE);
        h.extend_from_slice(ORBSHDR_MAGIC);
        h.push(version);
        h.extend_from_slice(&info_word(false, false, m_type, 0, code_len).to_le_bytes());
        h.push(0x03); // m_chunkUsageBaseOffsetInDW
        h.push(0x02); // m_numInputUsageSlots
        h.push(flags);
        h.push(0x00); // m_reserved3
        h.extend_from_slice(&hash0.to_le_bytes());
        h.extend_from_slice(&hash1.to_le_bytes());
        h.extend_from_slice(&crc32.to_le_bytes());
        assert_eq!(h.len(), SHADER_BINARY_INFO_SIZE);
        h
    }

    /// Assemble a complete `.sb` container: `code_len` bytes of stand-in GCN code
    /// (0x90 filler — not decoded here) followed by the 28-byte header.
    #[allow(clippy::too_many_arguments)]
    fn build_sb(
        m_type: u32,
        code_len: u32,
        flags: u8,
        hash0: u32,
        hash1: u32,
        crc32: u32,
    ) -> Vec<u8> {
        let mut blob = vec![0x90u8; code_len as usize];
        // Real GCN code ends in s_endpgm; validate_sb_candidate requires the last code dword to
        // be it, so stamp it (all these blobs are >= one dword long).
        let n = code_len as usize;
        blob[n - 4..n].copy_from_slice(&S_ENDPGM.to_le_bytes());
        blob.extend_from_slice(&build_header(
            1, m_type, code_len, flags, hash0, hash1, crc32,
        ));
        blob
    }

    #[test]
    fn parses_vs_blob_stage_length_and_range() {
        // AC #1: a hand-built VS OrbShdr blob → correct stage / length / code range.
        let base = 0x0020_0000u64;
        let code_len = 0x40u32;
        let blob = build_sb(
            1, /* VS */
            code_len,
            0x00,
            0xDEAD_BEEF,
            0xFEED_FACE,
            0x1234_5678,
        );
        let mem = BufMem::new(base, blob);

        let sb = parse_sb(base, &mem).expect("VS blob parses");
        assert_eq!(sb.stage, SbStage::Vertex);
        assert_eq!(sb.logical_stage(), Some(Stage::Vertex));
        assert_eq!(sb.info.code_len, code_len);
        assert_eq!(sb.code_range, base..(base + code_len as u64));
        assert_eq!(sb.info.shader_hash0, 0xDEAD_BEEF);
        assert_eq!(sb.info.shader_hash1, 0xFEED_FACE);
        assert_eq!(sb.info.crc32, 0x1234_5678);
        assert_eq!(sb.info.version, 1);
        assert!(sb.semantics.vertex_inputs.is_empty());
    }

    #[test]
    fn parses_ps_blob_and_srt_flags() {
        // AC #1: a hand-built PS OrbShdr blob with SRT flags set.
        let base = 0x0030_0000u64;
        let code_len = 0x20u32;
        // flags: m_isSrt | m_isExtendedUsageInfo (bits 0 and 2).
        let blob = build_sb(0 /* PS */, code_len, 0b0000_0101, 0, 0, 0);
        let mem = BufMem::new(base, blob);

        let sb = parse_sb(base, &mem).expect("PS blob parses");
        assert_eq!(sb.stage, SbStage::Pixel);
        assert_eq!(sb.logical_stage(), Some(Stage::Pixel));
        assert!(sb.info.is_srt);
        assert!(!sb.info.is_srt_used_info_valid);
        assert!(sb.info.is_extended_usage_info);
        assert_eq!(sb.info.chunk_usage_base_offset_dw, 0x03);
        assert_eq!(sb.info.num_input_usage_slots, 0x02);
    }

    #[test]
    fn parses_blob_with_input_usage_footer_gap() {
        // Real Orbis layout (RE'd from a retail title): the OrbShdr footer does not sit tight
        // against the code — a small input-usage/hash table separates them. Build code + a
        // 20-byte gap + header and assert the parse admits it and the code_range excludes the
        // gap (only the m_length code bytes).
        let base = 0x0080_0000u64;
        let code_len = 0x2cu32; // 44 bytes, as observed for a small VS
        let gap = 20usize;
        let mut blob = vec![0x90u8; code_len as usize];
        let n = code_len as usize;
        blob[n - 4..n].copy_from_slice(&S_ENDPGM.to_le_bytes()); // s_endpgm terminator
        blob.extend_from_slice(&vec![0x00u8; gap]); // input-usage/hash table stand-in
        blob.extend_from_slice(&build_header(
            7, 1, /* VS */
            code_len, 0x00, 0x11, 0x22, 0x33,
        ));
        let mem = BufMem::new(base, blob);

        let sb = parse_sb(base, &mem).expect("blob with a footer gap parses");
        assert_eq!(sb.stage, SbStage::Vertex);
        assert_eq!(sb.info.code_len, code_len);
        // code_range is exactly the code — the gap/footer are excluded.
        assert_eq!(sb.code_range, base..(base + code_len as u64));
    }

    #[test]
    fn footer_gap_beyond_bound_is_rejected() {
        // A gap larger than MAX_FOOTER_GAP means the located magic is not this code's footer
        // (a false positive / wrong shader) → clean LengthMismatch, not a bogus parse.
        let base = 0x0090_0000u64;
        let code_len = 0x10u32;
        let gap = (MAX_FOOTER_GAP as usize) + 4; // just past the bound
        let mut blob = vec![0x90u8; code_len as usize];
        blob.extend_from_slice(&vec![0x00u8; gap]);
        blob.extend_from_slice(&build_header(7, 1, code_len, 0x00, 0, 0, 0));
        let mem = BufMem::new(base, blob);
        assert_eq!(parse_sb(base, &mem), Err(SbParseError::LengthMismatch));
    }

    #[test]
    fn false_magic_within_gap_but_no_endpgm_is_rejected() {
        // The gap bound alone would accept a stray "OrbShdr" whose m_length places code_end
        // within MAX_FOOTER_GAP of the false magic. Splice a header at offset 0x20 with
        // m_length=0x20 (gap=0, so the gap check passes) into a blob whose "code" is 0x90
        // filler — NOT s_endpgm. The s_endpgm terminator guard must reject it; with no other
        // magic the parse fails cleanly instead of returning a bogus shader.
        let base = 0x00A0_0000u64;
        let mut blob = vec![0x90u8; 0x40];
        let hdr = build_header(7, 1 /* VS */, 0x20, 0x00, 0, 0, 0);
        blob[0x20..0x20 + SHADER_BINARY_INFO_SIZE].copy_from_slice(&hdr);
        let mem = BufMem::new(base, blob);
        assert_eq!(parse_sb(base, &mem), Err(SbParseError::LengthMismatch));
    }

    #[test]
    fn resumes_past_false_magic_to_real_header() {
        // task-67 AC#1: a stray "OrbShdr" inside the GCN code region must not abort the
        // parse; the scan resumes past the false candidate and finds the real header.
        let base = 0x0040_0000u64;
        let code_len = 0x40u32;
        let mut blob = build_sb(1 /* VS */, code_len, 0x00, 0xAA, 0xBB, 0xCC);
        // Splice a false magic into the code region, well before the real header at 0x40.
        blob[8..8 + ORBSHDR_MAGIC.len()].copy_from_slice(ORBSHDR_MAGIC);
        let mem = BufMem::new(base, blob);

        let sb = parse_sb(base, &mem).expect("resumes past the false magic to the real header");
        assert_eq!(sb.stage, SbStage::Vertex);
        assert_eq!(sb.code_range, base..(base + code_len as u64));
    }

    #[test]
    fn false_magic_with_no_valid_header_rejects() {
        // task-67 AC#2: a blob whose only "OrbShdr" is a false positive (nothing validates)
        // still rejects cleanly instead of returning a bogus shader.
        let base = 0x0050_0000u64;
        let mut blob = vec![0x90u8; 64];
        blob[8..8 + ORBSHDR_MAGIC.len()].copy_from_slice(ORBSHDR_MAGIC);
        let mem = BufMem::new(base, blob);

        assert!(
            parse_sb(base, &mem).is_err(),
            "a false-only magic must not parse to a shader"
        );
    }

    #[test]
    fn all_known_stages_decode() {
        for (val, want) in [
            (0u32, SbStage::Pixel),
            (1, SbStage::Vertex),
            (2, SbStage::Export),
            (3, SbStage::Local),
            (4, SbStage::Compute),
            (5, SbStage::Geometry),
            (7, SbStage::Hull),
            (8, SbStage::Domain),
        ] {
            let base = 0x0100_0000u64;
            let blob = build_sb(val, 0x10, 0, 0, 0, 0);
            let mem = BufMem::new(base, blob);
            let sb = parse_sb(base, &mem).unwrap();
            assert_eq!(sb.stage, want, "m_type {val}");
        }
    }

    #[test]
    fn unknown_stage_is_rejected() {
        // m_type 6 is kUnknown; must be a clean Err, not a decode.
        let base = 0x0040_0000u64;
        let blob = build_sb(6, 0x10, 0, 0, 0, 0);
        let mem = BufMem::new(base, blob);
        assert_eq!(parse_sb(base, &mem), Err(SbParseError::UnknownStage(6)));
    }

    #[test]
    fn bad_magic_is_rejected_no_panic() {
        // AC #2: garbage with no OrbShdr magic → MagicNotFound, no panic/OOB.
        let base = 0x0050_0000u64;
        let blob = vec![0xABu8; 0x100];
        let mem = BufMem::new(base, blob);
        assert_eq!(parse_sb(base, &mem), Err(SbParseError::MagicNotFound));
    }

    #[test]
    fn truncated_header_is_rejected_no_panic() {
        // AC #2: a magic at the very end with no room for the 28-byte header.
        let base = 0x0060_0000u64;
        // Just the magic and a couple bytes — header runs past the buffer.
        let mut blob = vec![0x90u8; 8];
        blob.extend_from_slice(ORBSHDR_MAGIC);
        blob.extend_from_slice(&[0u8; 4]); // < 28 bytes of header
        let mem = BufMem::new(base, blob);
        // Either the magic isn't reachable with a full header (MagicNotFound) or the
        // read faults (Truncated/MemoryFault) — all clean, no panic.
        let err = parse_sb(base, &mem).unwrap_err();
        assert!(matches!(
            err,
            SbParseError::MagicNotFound | SbParseError::Truncated | SbParseError::MemoryFault
        ));
    }

    #[test]
    fn length_past_buffer_is_rejected_no_panic() {
        // AC #2: header claims a code_len far larger than the real code region, so
        // the found magic sits at the wrong offset → LengthMismatch (not OOB).
        let base = 0x0070_0000u64;
        // Real code region is 0x10 bytes, but stamp m_length = 0x1000 in the header.
        let code_len_real = 0x10u32;
        let lie_len = 0x1000u32;
        let mut blob = vec![0x90u8; code_len_real as usize];
        blob.extend_from_slice(&build_header(1, 1, lie_len, 0, 0, 0, 0));
        let mem = BufMem::new(base, blob);
        // Magic is found at base+0x10, but code_start+lie_len != header_addr.
        assert_eq!(parse_sb(base, &mem), Err(SbParseError::LengthMismatch));
    }

    #[test]
    fn garbage_before_magic_still_found_and_validated() {
        // The scan must find a magic offset by real code, and reject if the length
        // invariant fails. Here code_len matches, so it succeeds despite the code
        // bytes being arbitrary (not decoded).
        let base = 0x0080_0000u64;
        let code_len = 0x88u32;
        let mut blob: Vec<u8> = (0..code_len as u8)
            .cycle()
            .take(code_len as usize)
            .collect();
        let n = code_len as usize;
        blob[n - 4..n].copy_from_slice(&S_ENDPGM.to_le_bytes()); // s_endpgm terminator
        blob.extend_from_slice(&build_header(1, 1, code_len, 0, 0, 0, 0));
        let mem = BufMem::new(base, blob);
        let sb = parse_sb(base, &mem).unwrap();
        assert_eq!(sb.code_range, base..(base + code_len as u64));
    }

    #[test]
    fn unmapped_address_faults_cleanly() {
        // AC #2: a code_start with nothing mapped → clean Err (MagicNotFound via the
        // failed reads), never a panic.
        let mem = BufMem::new(0x0090_0000, vec![0u8; 0x40]);
        // Ask to parse at a base far outside the buffer.
        let err = parse_sb(0x0F00_0000, &mem).unwrap_err();
        assert!(matches!(
            err,
            SbParseError::MagicNotFound | SbParseError::MemoryFault
        ));
    }

    #[test]
    fn pgm_addr_derivation() {
        // AC #3: PGM_LO/HI → code_start = ((hi<<32)|lo) << 8. This is the fixture
        // the draw path feeds code_start (via pgm_addr) into parse_sb.
        assert_eq!(pgm_addr(0x0000_2000, 0x0000_0000), 0x0020_0000);
        // Split across both registers: code at 0x1_0000_0000 → >>8 = 0x0100_0000.
        assert_eq!(pgm_addr(0x0100_0000, 0x0000_0000), 0x0000_0001_0000_0000);
        // High register contributes the top bits.
        assert_eq!(pgm_addr(0x0000_0000, 0x0000_0001), 0x0000_0100_0000_0000);
        // Round-trip a realistic aligned address.
        let code = 0x0000_0008_ABCD_EF00u64;
        let shifted = code >> 8;
        let lo = (shifted & 0xFFFF_FFFF) as u32;
        let hi = (shifted >> 32) as u32;
        assert_eq!(pgm_addr(lo, hi), code);
    }

    #[test]
    fn pgm_addr_then_parse_end_to_end() {
        // AC #3: full P4-09 path — derive code_start from PGM regs, then parse.
        let code_start = 0x0020_0000u64;
        let shifted = code_start >> 8;
        let pgm_lo = (shifted & 0xFFFF_FFFF) as u32;
        let pgm_hi = (shifted >> 32) as u32;
        assert_eq!(pgm_addr(pgm_lo, pgm_hi), code_start);

        let code_len = 0x30u32;
        let blob = build_sb(1, code_len, 0, 0xA, 0xB, 0xC);
        let mem = BufMem::new(code_start, blob);
        let sb = parse_sb(pgm_addr(pgm_lo, pgm_hi), &mem).unwrap();
        assert_eq!(sb.stage, SbStage::Vertex);
        assert_eq!(sb.code_range, code_start..(code_start + code_len as u64));
    }

    #[test]
    fn parses_vs_semantic_tables_from_block() {
        // The gnmx-block semantic parsers round-trip a hand-built input+export array.
        let base = 0x00A0_0000u64;
        let mut buf = Vec::new();
        // Two VertexInputSemantic (4 bytes each): (sem, vgpr, size, rsv).
        buf.extend_from_slice(&[0x00, 0x04, 0x04, 0x00]);
        buf.extend_from_slice(&[0x01, 0x08, 0x02, 0x00]);
        let export_off = buf.len() as u64;
        // Two VertexExportSemantic (2 bytes each): sem, (outIndex:5|rsv:1|f16:2).
        buf.extend_from_slice(&[0x05, 0x01]); // out_index 1, f16 0
        buf.extend_from_slice(&[0x06, 0b1100_0011]); // out_index 3, f16 3
        let mem = BufMem::new(base, buf);

        let s = parse_vs_semantics(&mem, base, 2, base + export_off, 2).unwrap();
        assert_eq!(s.vertex_inputs.len(), 2);
        assert_eq!(
            s.vertex_inputs[1],
            VertexInputSemantic {
                semantic: 1,
                vgpr: 8,
                size_in_elements: 2
            }
        );
        assert_eq!(s.vertex_exports[0].out_index, 1);
        assert_eq!(s.vertex_exports[1].out_index, 3);
        assert_eq!(s.vertex_exports[1].export_f16, 3);
    }

    #[test]
    fn parses_ps_semantic_tables_from_block() {
        let base = 0x00B0_0000u64;
        let mut buf = Vec::new();
        // One PixelInputSemantic (2 bytes, u16 LE): sem=7, default=1, flat=1.
        let w: u16 = 7 | (1 << 8) | (1 << 10);
        buf.extend_from_slice(&w.to_le_bytes());
        let map_off = buf.len() as u64;
        // One PixelSemanticMapping (4 bytes): out_index=2, in_index=0.
        buf.extend_from_slice(&[0x02, 0x00, 0x00, 0x00]);
        let mem = BufMem::new(base, buf);

        let s = parse_ps_semantics(&mem, base, 1, base + map_off, 1).unwrap();
        assert_eq!(s.pixel_inputs.len(), 1);
        assert_eq!(s.pixel_inputs[0].semantic, 7);
        assert_eq!(s.pixel_inputs[0].default_value, 1);
        assert!(s.pixel_inputs[0].is_flat_shaded);
        assert!(!s.pixel_inputs[0].is_linear);
        assert_eq!(s.pixel_mappings[0].out_index, 2);
        assert_eq!(s.pixel_mappings[0].in_index, 0);
    }

    #[test]
    fn semantic_table_out_of_range_faults_cleanly() {
        // A count that runs past the buffer must be a clean MemoryFault, not OOB.
        let base = 0x00C0_0000u64;
        let mem = BufMem::new(base, vec![0u8; 8]);
        let err = parse_vs_semantics(&mem, base, 100, base, 0).unwrap_err();
        assert_eq!(err, SbParseError::MemoryFault);
    }
}
