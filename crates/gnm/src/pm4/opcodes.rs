//! PM4 opcode tables / `IT_*` constants and register-base constants
//! (doc-2 §1, §5). Vulkan-free, no execution.
//!
//! Opcode values are the AMD PM4 Type-3 opcode enumeration for GFX6/7 (PS4
//! Liverpool is GCN2 / Sea Islands / GFX7). Every value is pinned to its published
//! AMD-hardware constant by the `pm4_opcodes_match_amd_oracle` test below: Mesa
//! `src/amd/common/sid.h` `PKT3_*` (corroborated by the Linux kernel
//! `drivers/gpu/drm/radeon/{sid,cikd}.h` `PACKET3_*`). The register-base constants are
//! the GFX6/7 CP register windows the SET_*_REG packets add their per-entry offset to
//! (Mesa/kernel `SI_*_REG_OFFSET`). Register byte addresses cited below are the AMD MMIO
//! offsets machine-listed in Mesa `src/amd/registers/gfx6.json` / `gfx7.json`.
//!
//! **This file is the single source of truth for the shared PM4 `IT_*` opcodes
//! and GFX6 register-window bases.** The guest PM4 test corpus
//! (`examples/ps4-pm4-test/ps4-pm4-test/main.c`) re-`#define`s a handful of
//! these so it can hand-emit named packets — that file mirrors these values, it
//! does not own them. The `corpus_mirror_matches_opcodes` test below reads the
//! corpus at test time and fails if either side drifts.

/// PM4 Type-3 IT_* opcodes (bits [15:8] of a Type-3 header). Each is the AMD
/// hardware opcode published as Mesa `PKT3_*` (`src/amd/common/sid.h`), corroborated
/// by kernel `PACKET3_*` (radeon `sid.h`/`cikd.h`); pinned by `pm4_opcodes_match_amd_oracle`.
pub mod op {
    pub const IT_NOP: u8 = 0x10;
    pub const IT_CLEAR_STATE: u8 = 0x12;
    pub const IT_INDEX_BUFFER_SIZE: u8 = 0x13;
    pub const IT_DISPATCH_DIRECT: u8 = 0x15;
    pub const IT_DISPATCH_INDIRECT: u8 = 0x16;
    pub const IT_INDEX_BASE: u8 = 0x26;
    pub const IT_DRAW_INDEX_2: u8 = 0x27;
    pub const IT_CONTEXT_CONTROL: u8 = 0x28;
    pub const IT_INDEX_TYPE: u8 = 0x2A;
    pub const IT_DRAW_INDEX_AUTO: u8 = 0x2D;
    pub const IT_NUM_INSTANCES: u8 = 0x2F;
    pub const IT_DRAW_INDEX_OFFSET_2: u8 = 0x35;
    pub const IT_WRITE_DATA: u8 = 0x37;
    pub const IT_WAIT_REG_MEM: u8 = 0x3C;
    pub const IT_INDIRECT_BUFFER: u8 = 0x3F;
    pub const IT_PFP_SYNC_ME: u8 = 0x42;
    pub const IT_EVENT_WRITE: u8 = 0x46;
    pub const IT_EVENT_WRITE_EOP: u8 = 0x47;
    pub const IT_EVENT_WRITE_EOS: u8 = 0x48;
    pub const IT_DMA_DATA: u8 = 0x50;
    pub const IT_ACQUIRE_MEM: u8 = 0x58;
    pub const IT_SET_CONFIG_REG: u8 = 0x68;
    pub const IT_SET_CONTEXT_REG: u8 = 0x69;
    pub const IT_SET_SH_REG: u8 = 0x76;
    pub const IT_SET_UCONFIG_REG: u8 = 0x79;
}

/// GFX6 CP register windows (dword offsets). A SET_*_REG packet's first body
/// dword is the register offset relative to the matching base; the absolute
/// register index is `base + reg_offset`.
pub mod reg_base {
    pub const CONFIG: u32 = 0x2000;
    pub const CONTEXT: u32 = 0xA000;
    pub const SH: u32 = 0x2C00;
    pub const UCONFIG: u32 = 0xC000;
}

/// Absolute SH-bank register indices for the graphics VS/PS shader-program setup
/// (GFX6/7 `SPI_SHADER_*`, dword indices). These are the standard AMD offsets: the
/// SH window base [`reg_base::SH`] + the per-stage sub-offset = `(MMIO byte address -
/// 0xB000) / 4`, where the byte addresses are the AMD MMIO offsets machine-listed in
/// Mesa `src/amd/registers/gfx6.json` (`SPI_SHADER_PGM_LO_PS` at `0xB020`, `_VS` at
/// `0xB120`). The guest PM4 test corpus mirrors `PGM_LO_VS = 0x48` / `_PS = 0x08` in
/// `examples/ps4-pm4-test/ps4-pm4-test/main.c`. The draw path reads `PGM_LO/HI` to
/// derive the `.sb` code address (doc-2 §5) and `PGM_RSRC1/2` for the GPR / user-SGPR
/// counts.
pub mod sh_reg {
    use super::reg_base::SH;

    /// PS `SPI_SHADER_PGM_LO_PS` — low 32 bits of the pixel program address (>>8).
    pub const SPI_SHADER_PGM_LO_PS: u32 = SH + 0x08;
    /// PS `SPI_SHADER_PGM_HI_PS` — high bits of the pixel program address (>>8).
    pub const SPI_SHADER_PGM_HI_PS: u32 = SH + 0x09;
    /// PS `SPI_SHADER_PGM_RSRC1_PS` — VGPR/SGPR counts (see [`super::pgm_rsrc`]).
    pub const SPI_SHADER_PGM_RSRC1_PS: u32 = SH + 0x0A;
    /// PS `SPI_SHADER_PGM_RSRC2_PS` — user-SGPR count etc.
    pub const SPI_SHADER_PGM_RSRC2_PS: u32 = SH + 0x0B;

    /// VS `SPI_SHADER_PGM_LO_VS` — low 32 bits of the vertex program address (>>8).
    pub const SPI_SHADER_PGM_LO_VS: u32 = SH + 0x48;
    /// VS `SPI_SHADER_PGM_HI_VS` — high bits of the vertex program address (>>8).
    pub const SPI_SHADER_PGM_HI_VS: u32 = SH + 0x49;
    /// VS `SPI_SHADER_PGM_RSRC1_VS` — VGPR/SGPR counts (see [`super::pgm_rsrc`]).
    pub const SPI_SHADER_PGM_RSRC1_VS: u32 = SH + 0x4A;
    /// VS `SPI_SHADER_PGM_RSRC2_VS` — user-SGPR count etc.
    pub const SPI_SHADER_PGM_RSRC2_VS: u32 = SH + 0x4B;

    /// PS `SPI_SHADER_PGM_RSRC3_PS` — CU-enable / wave-limit mask. SH byte address
    /// `0xB01C` (Mesa `registers/gfx7.json` `SPI_SHADER_PGM_RSRC3_PS`; GFX7) → SH
    /// dword `0x1C/4 = 0x07`.
    pub const SPI_SHADER_PGM_RSRC3_PS: u32 = SH + 0x07;
    /// VS `SPI_SHADER_PGM_RSRC3_VS` — CU-enable / wave-limit mask. SH byte address
    /// `0xB118` (Mesa `registers/gfx7.json` `SPI_SHADER_PGM_RSRC3_VS`; GFX7) → SH
    /// dword `0x118/4 = 0x46`.
    pub const SPI_SHADER_PGM_RSRC3_VS: u32 = SH + 0x46;

    /// Slot 0 of the 16-entry `SPI_SHADER_USER_DATA_PS_*` block — the user-SGPR words
    /// the driver preloads before the PS runs (V#/T#/S# pointers, inline constants).
    /// SH byte address `0xB030` (Mesa `registers/gfx6.json` `SPI_SHADER_USER_DATA_PS_0`)
    /// → SH dword `0x30/4 = 0x0C`; slot `i` is `SPI_SHADER_USER_DATA_PS_0 + i`.
    pub const SPI_SHADER_USER_DATA_PS_0: u32 = SH + 0x0C;
    /// Slot 0 of the 16-entry `SPI_SHADER_USER_DATA_VS_*` block. SH byte address
    /// `0xB130` (Mesa `registers/gfx6.json` `SPI_SHADER_USER_DATA_VS_0`) → SH dword
    /// `0x130/4 = 0x4C`; slot `i` is `SPI_SHADER_USER_DATA_VS_0 + i`.
    pub const SPI_SHADER_USER_DATA_VS_0: u32 = SH + 0x4C;

    /// Number of user-SGPR slots in a `SPI_SHADER_USER_DATA_*` block (GFX6: 16).
    pub const USER_DATA_SLOTS: u32 = 16;
}

/// Absolute CONTEXT-bank register indices for the graphics VS/PS pipeline state that
/// the Gnm `VsStageRegisters` / `PsStageRegisters` structs carry alongside the SH
/// shader-program run. Offsets are the standard AMD SI/GFX6 context registers: the
/// dword offset is `(byte address - 0x28000) / 4`, where the `0x028xxx` byte addresses
/// are the AMD MMIO offsets machine-listed in Mesa `src/amd/registers/gfx6.json` (also
/// in kernel `drivers/gpu/drm/radeon/sid.h`). Corroborated by the in-repo corpus, whose
/// `CONTEXT_SPI_SHADER_COL_FORMAT 0x01C5`
/// (`examples/ps4-pm4-test/ps4-pm4-test/main.c`) matches `SPI_SHADER_COL_FORMAT` below.
pub mod context_reg {
    use super::reg_base::CONTEXT;

    /// `SPI_VS_OUT_CONFIG` — VS output parameter count (`R_0286C4`).
    pub const SPI_VS_OUT_CONFIG: u32 = CONTEXT + 0x1B1;
    /// `SPI_PS_INPUT_ENA` — PS interpolant enable mask (`R_0286CC`).
    pub const SPI_PS_INPUT_ENA: u32 = CONTEXT + 0x1B3;
    /// `SPI_PS_INPUT_ADDR` — PS interpolant address mask (`R_0286D0`).
    pub const SPI_PS_INPUT_ADDR: u32 = CONTEXT + 0x1B4;
    /// `SPI_PS_IN_CONTROL` — PS input count / control (`R_0286D8`).
    pub const SPI_PS_IN_CONTROL: u32 = CONTEXT + 0x1B6;
    /// `SPI_BARYC_CNTL` — barycentric control (`R_0286E0`).
    pub const SPI_BARYC_CNTL: u32 = CONTEXT + 0x1B8;
    /// `SPI_SHADER_POS_FORMAT` — VS position-export format (`R_02870C`).
    pub const SPI_SHADER_POS_FORMAT: u32 = CONTEXT + 0x1C3;
    /// `SPI_SHADER_Z_FORMAT` — PS depth-export format (`R_028710`).
    pub const SPI_SHADER_Z_FORMAT: u32 = CONTEXT + 0x1C4;
    /// `SPI_SHADER_COL_FORMAT` — PS color-export format (`R_028714`; corpus-mirrored).
    pub const SPI_SHADER_COL_FORMAT: u32 = CONTEXT + 0x1C5;
    /// `SPI_PS_INPUT_CNTL_0` — first of 32 PS input-slot descriptors (`R_028644`).
    /// Slot `n`'s `OFFSET` field (bits [4:0]) names which VS export PARAMETER feeds PS
    /// attribute `n`; the mapping is NOT the identity the recompiler currently assumes.
    pub const SPI_PS_INPUT_CNTL_0: u32 = CONTEXT + 0x191;
    /// `CB_TARGET_MASK` — per-MRT colour write-enable mask (byte `0x028238`, Mesa
    /// `registers/gfx6.json` `CB_TARGET_MASK`). MRT0's enables are bits `[3:0]` = R,G,B,A.
    pub const CB_TARGET_MASK: u32 = CONTEXT + 0x08E;
    /// `CB_SHADER_MASK` — per-MRT output-component mask (`R_02823C`).
    pub const CB_SHADER_MASK: u32 = CONTEXT + 0x08F;
    /// `DB_SHADER_CONTROL` — depth/stencil shader control (`R_02880C`).
    pub const DB_SHADER_CONTROL: u32 = CONTEXT + 0x203;
    /// `PA_CL_VS_OUT_CNTL` — clip/cull output enables (`R_02881C`).
    pub const PA_CL_VS_OUT_CNTL: u32 = CONTEXT + 0x207;

    /// `CB_COLOR0_BASE` — MRT0 color-target base (256-byte units, `<<8` for the byte
    /// address), `R_028C60`. Untrusted (guest-programmed): the byte address is only
    /// dereferenced through the bounded read seam.
    pub const CB_COLOR0_BASE: u32 = CONTEXT + 0x318;
    /// `CB_COLOR0_PITCH` — MRT0 tile-max pitch (`R_028C64`).
    pub const CB_COLOR0_PITCH: u32 = CONTEXT + 0x319;
    /// `CB_COLOR0_SLICE` — MRT0 tile-max slice (`R_028C68`).
    pub const CB_COLOR0_SLICE: u32 = CONTEXT + 0x31A;
    /// `CB_COLOR0_VIEW` — MRT0 array-slice range (`R_028C6C`).
    pub const CB_COLOR0_VIEW: u32 = CONTEXT + 0x31B;
    /// `CB_COLOR0_INFO` — MRT0 format / number-type / compression flags (`R_028C70`).
    pub const CB_COLOR0_INFO: u32 = CONTEXT + 0x31C;
    /// `CB_COLOR0_ATTRIB` — MRT0 tile-mode index / fmask-tile / dimensions (`R_028C74`).
    pub const CB_COLOR0_ATTRIB: u32 = CONTEXT + 0x31D;

    /// `PA_CL_VPORT_XSCALE` — viewport-0 X scale (f32 bits), `R_02843C`.
    pub const PA_CL_VPORT_XSCALE: u32 = CONTEXT + 0x10F;
    /// `PA_CL_VPORT_XOFFSET` — viewport-0 X offset (f32 bits), `R_028440`.
    pub const PA_CL_VPORT_XOFFSET: u32 = CONTEXT + 0x110;
    /// `PA_CL_VPORT_YSCALE` — viewport-0 Y scale (f32 bits), `R_028444`.
    pub const PA_CL_VPORT_YSCALE: u32 = CONTEXT + 0x111;
    /// `PA_CL_VPORT_YOFFSET` — viewport-0 Y offset (f32 bits), `R_028448`.
    pub const PA_CL_VPORT_YOFFSET: u32 = CONTEXT + 0x112;

    /// `PA_SC_SCREEN_SCISSOR_TL` — screen scissor top-left (x[15:0], y[31:16]), `R_028030`.
    pub const PA_SC_SCREEN_SCISSOR_TL: u32 = CONTEXT + 0x00C;
    /// `PA_SC_SCREEN_SCISSOR_BR` — screen scissor bottom-right (x[15:0], y[31:16]), `R_028034`.
    pub const PA_SC_SCREEN_SCISSOR_BR: u32 = CONTEXT + 0x00D;

    /// `CB_BLEND0_CONTROL` — MRT0 blend equation / factors (`R_028780`).
    pub const CB_BLEND0_CONTROL: u32 = CONTEXT + 0x1E0;
    /// `CB_COLOR_CONTROL` — global color-buffer mode / ROP (`R_028808`).
    pub const CB_COLOR_CONTROL: u32 = CONTEXT + 0x202;

    /// `DB_DEPTH_CONTROL` — depth-test enable / compare / stencil (`R_028800`).
    pub const DB_DEPTH_CONTROL: u32 = CONTEXT + 0x200;
    /// `DB_Z_INFO` — depth-surface format / tiling (`R_028040`).
    pub const DB_Z_INFO: u32 = CONTEXT + 0x010;
}

/// Absolute UCONFIG-bank register indices the draw path reads (dword indices,
/// [`reg_base::UCONFIG`] window).
pub mod uconfig {
    use super::reg_base::UCONFIG;

    /// `VGT_PRIMITIVE_TYPE` — the input-assembly primitive type the next draw
    /// rasterizes. On GFX7/Liverpool this is UCONFIG byte `0x030908` (Mesa
    /// `registers/gfx7.json` `VGT_PRIMITIVE_TYPE`; the GFX6 location `0x8958` moved into
    /// the UCONFIG window on GFX7) → uconfig dword `(0x30908 - 0x30000)/4 = 0x242`, i.e.
    /// `UCONFIG + 0x242`. Values are the `DI_PT_*` enum; see [`di_pt`](super::di_pt).
    pub const VGT_PRIMITIVE_TYPE: u32 = UCONFIG + 0x242;
}

/// `VGT_PRIMITIVE_TYPE` values (`DI_PT_*`), the ones the draw path distinguishes.
pub mod di_pt {
    /// Three vertices per triangle — the common case.
    pub const TRILIST: u32 = 0x04;
    /// Three vertices per RECTANGLE: `p0`, `p1`, `p2` name three corners and the
    /// hardware synthesizes the fourth as `p2 + p1 - p0`. Has no Vulkan equivalent;
    /// PS4 titles issue full-screen fills/clears with it (Celeste's bloom-target
    /// clears, task-184).
    pub const RECTLIST: u32 = 0x11;
}

/// Decoders for the GFX6/7 `SPI_SHADER_PGM_RSRC1/2` bitfields (GPR / user-SGPR
/// counts). These are the register-truth inputs the draw path snapshots into a
/// shader's resource footprint (doc-2 §5). The bit positions are the AMD hardware
/// layout machine-listed in Mesa `src/amd/registers/gfx6.json`
/// (`SPI_SHADER_PGM_RSRC1_PS`: `VGPRS` bits [0:5], `SGPRS` bits [6:9];
/// `SPI_SHADER_PGM_RSRC2_PS`: `USER_SGPR` bits [1:5]) and described in the CI-ISA
/// PGM_RSRC register section:
///
/// - `PGM_RSRC1.VGPRS` (bits [5:0]) is `(vgprs_used - 1) / 4`, so the allocated
///   count is `(field + 1) * 4`.
/// - `PGM_RSRC1.SGPRS` (bits [9:6]) is `(sgprs_used - 1) / 8`, so the allocated
///   count is `(field + 1) * 8`.
/// - `PGM_RSRC2.USER_SGPR` (bits [5:1]) is the user-SGPR count verbatim.
pub mod pgm_rsrc {
    /// Allocated VGPR count from `PGM_RSRC1` (`(VGPRS + 1) * 4`).
    pub fn num_vgprs(rsrc1: u32) -> u32 {
        ((rsrc1 & 0x3F) + 1) * 4
    }
    /// Allocated SGPR count from `PGM_RSRC1` (`(SGPRS + 1) * 8`).
    pub fn num_sgprs(rsrc1: u32) -> u32 {
        (((rsrc1 >> 6) & 0xF) + 1) * 8
    }
    /// User-SGPR count from `PGM_RSRC2` (`USER_SGPR`, bits [5:1]).
    pub fn num_user_sgprs(rsrc2: u32) -> u32 {
        (rsrc2 >> 1) & 0x1F
    }
}

/// Build a Type-3 PM4 header dword: type=3 in [31:30], `opcode` in [15:8], and
/// `count = body_len - 1` in [29:16] (the CP's "count is one less than the body
/// dword length" convention). This is the AMD PM4 Type-3 header layout — Mesa
/// `src/amd/common/sid.h`: `PKT3(op, count, pred) = PKT_TYPE_S(3) | PKT_COUNT_S(count)
/// | PKT3_IT_OPCODE_S(op)`, where `PKT_TYPE_S(x) = x << 30`, `PKT_COUNT_S(x) = x << 16`,
/// `PKT3_IT_OPCODE_S(x) = x << 8`. `body_len` is the number of dwords that follow the
/// header. The single source of truth for the Type-3 header encoding used by the
/// emitter, decoder round-trip tests, and executor tests.
pub fn t3_header(opcode: u8, body_len: usize) -> u32 {
    let count = (body_len - 1) as u32 & 0x3FFF;
    (0b11 << 30) | (count << 16) | ((opcode as u32) << 8)
}

/// Human-readable name for a Type-3 opcode, or `None` if unknown. Used by the
/// trace renderer; unknown opcodes fall back to their raw value (never fatal).
pub fn name(opcode: u8) -> Option<&'static str> {
    use op::*;
    Some(match opcode {
        IT_NOP => "IT_NOP",
        IT_CLEAR_STATE => "IT_CLEAR_STATE",
        IT_INDEX_BUFFER_SIZE => "IT_INDEX_BUFFER_SIZE",
        IT_DISPATCH_DIRECT => "IT_DISPATCH_DIRECT",
        IT_DISPATCH_INDIRECT => "IT_DISPATCH_INDIRECT",
        IT_INDEX_BASE => "IT_INDEX_BASE",
        IT_DRAW_INDEX_2 => "IT_DRAW_INDEX_2",
        IT_CONTEXT_CONTROL => "IT_CONTEXT_CONTROL",
        IT_INDEX_TYPE => "IT_INDEX_TYPE",
        IT_DRAW_INDEX_AUTO => "IT_DRAW_INDEX_AUTO",
        IT_NUM_INSTANCES => "IT_NUM_INSTANCES",
        IT_DRAW_INDEX_OFFSET_2 => "IT_DRAW_INDEX_OFFSET_2",
        IT_WRITE_DATA => "IT_WRITE_DATA",
        IT_WAIT_REG_MEM => "IT_WAIT_REG_MEM",
        IT_INDIRECT_BUFFER => "IT_INDIRECT_BUFFER",
        IT_PFP_SYNC_ME => "IT_PFP_SYNC_ME",
        IT_EVENT_WRITE => "IT_EVENT_WRITE",
        IT_EVENT_WRITE_EOP => "IT_EVENT_WRITE_EOP",
        IT_EVENT_WRITE_EOS => "IT_EVENT_WRITE_EOS",
        IT_DMA_DATA => "IT_DMA_DATA",
        IT_ACQUIRE_MEM => "IT_ACQUIRE_MEM",
        IT_SET_CONFIG_REG => "IT_SET_CONFIG_REG",
        IT_SET_CONTEXT_REG => "IT_SET_CONTEXT_REG",
        IT_SET_SH_REG => "IT_SET_SH_REG",
        IT_SET_UCONFIG_REG => "IT_SET_UCONFIG_REG",
        _ => return None,
    })
}

/// The hand-maintained catalog of every pipeline-affecting register this file DEFINES, as
/// `(absolute index, name)` (task-183). Rust has no reflection, so this list is the single
/// source of truth for two things:
///
/// * [`reg_name`]'s scalar resolution reads names from here — a register that is not
///   catalogued cannot be named or dumped;
/// * the `pipeline_register_coverage_is_audited` test partitions this catalog into the
///   registers the GNM→pipeline derivation CONSUMES and an `IGNORED_WITH_REASON` allow-list,
///   and fails if any entry is in neither. A newly-added-but-unread register therefore trips
///   the test until it is modelled or given a cited reason it is safe to ignore — the
///   anti-drift guard for the "guest wrote it, nothing read it" bug class (task-179).
///
/// Ordering mirrors the constant definitions. The two `SPI_SHADER_USER_DATA_*` blocks and the
/// `SPI_PS_INPUT_CNTL_*` block are represented by their slot-0 base; [`reg_name`] names the
/// remaining slots by range. Register semantics are the AMD GFX6 (Liverpool) definitions in
/// Mesa `src/amd/registers/gfx6.json` cited on each constant above.
pub mod reg {
    use super::{context_reg as ctx, sh_reg as sh, uconfig};

    /// Every pipeline register this file defines, `(index, name)`. See the module doc.
    pub const ALL_PIPELINE_REGS: &[(u32, &str)] = &[
        // --- SH bank: shader-program setup ---
        (sh::SPI_SHADER_PGM_LO_PS, "SPI_SHADER_PGM_LO_PS"),
        (sh::SPI_SHADER_PGM_HI_PS, "SPI_SHADER_PGM_HI_PS"),
        (sh::SPI_SHADER_PGM_RSRC1_PS, "SPI_SHADER_PGM_RSRC1_PS"),
        (sh::SPI_SHADER_PGM_RSRC2_PS, "SPI_SHADER_PGM_RSRC2_PS"),
        (sh::SPI_SHADER_PGM_RSRC3_PS, "SPI_SHADER_PGM_RSRC3_PS"),
        (sh::SPI_SHADER_PGM_LO_VS, "SPI_SHADER_PGM_LO_VS"),
        (sh::SPI_SHADER_PGM_HI_VS, "SPI_SHADER_PGM_HI_VS"),
        (sh::SPI_SHADER_PGM_RSRC1_VS, "SPI_SHADER_PGM_RSRC1_VS"),
        (sh::SPI_SHADER_PGM_RSRC2_VS, "SPI_SHADER_PGM_RSRC2_VS"),
        (sh::SPI_SHADER_PGM_RSRC3_VS, "SPI_SHADER_PGM_RSRC3_VS"),
        (sh::SPI_SHADER_USER_DATA_PS_0, "SPI_SHADER_USER_DATA_PS_0"),
        (sh::SPI_SHADER_USER_DATA_VS_0, "SPI_SHADER_USER_DATA_VS_0"),
        // --- CONTEXT bank: VS/PS pipeline state ---
        (ctx::SPI_VS_OUT_CONFIG, "SPI_VS_OUT_CONFIG"),
        (ctx::SPI_PS_INPUT_ENA, "SPI_PS_INPUT_ENA"),
        (ctx::SPI_PS_INPUT_ADDR, "SPI_PS_INPUT_ADDR"),
        (ctx::SPI_PS_IN_CONTROL, "SPI_PS_IN_CONTROL"),
        (ctx::SPI_BARYC_CNTL, "SPI_BARYC_CNTL"),
        (ctx::SPI_SHADER_POS_FORMAT, "SPI_SHADER_POS_FORMAT"),
        (ctx::SPI_SHADER_Z_FORMAT, "SPI_SHADER_Z_FORMAT"),
        (ctx::SPI_SHADER_COL_FORMAT, "SPI_SHADER_COL_FORMAT"),
        (ctx::SPI_PS_INPUT_CNTL_0, "SPI_PS_INPUT_CNTL_0"),
        (ctx::CB_TARGET_MASK, "CB_TARGET_MASK"),
        (ctx::CB_SHADER_MASK, "CB_SHADER_MASK"),
        (ctx::DB_SHADER_CONTROL, "DB_SHADER_CONTROL"),
        (ctx::PA_CL_VS_OUT_CNTL, "PA_CL_VS_OUT_CNTL"),
        (ctx::CB_COLOR0_BASE, "CB_COLOR0_BASE"),
        (ctx::CB_COLOR0_PITCH, "CB_COLOR0_PITCH"),
        (ctx::CB_COLOR0_SLICE, "CB_COLOR0_SLICE"),
        (ctx::CB_COLOR0_VIEW, "CB_COLOR0_VIEW"),
        (ctx::CB_COLOR0_INFO, "CB_COLOR0_INFO"),
        (ctx::CB_COLOR0_ATTRIB, "CB_COLOR0_ATTRIB"),
        (ctx::PA_CL_VPORT_XSCALE, "PA_CL_VPORT_XSCALE"),
        (ctx::PA_CL_VPORT_XOFFSET, "PA_CL_VPORT_XOFFSET"),
        (ctx::PA_CL_VPORT_YSCALE, "PA_CL_VPORT_YSCALE"),
        (ctx::PA_CL_VPORT_YOFFSET, "PA_CL_VPORT_YOFFSET"),
        (ctx::PA_SC_SCREEN_SCISSOR_TL, "PA_SC_SCREEN_SCISSOR_TL"),
        (ctx::PA_SC_SCREEN_SCISSOR_BR, "PA_SC_SCREEN_SCISSOR_BR"),
        (ctx::CB_BLEND0_CONTROL, "CB_BLEND0_CONTROL"),
        (ctx::CB_COLOR_CONTROL, "CB_COLOR_CONTROL"),
        (ctx::DB_DEPTH_CONTROL, "DB_DEPTH_CONTROL"),
        (ctx::DB_Z_INFO, "DB_Z_INFO"),
        // --- UCONFIG bank ---
        (uconfig::VGT_PRIMITIVE_TYPE, "VGT_PRIMITIVE_TYPE"),
    ];
}

/// Human-readable name for an ABSOLUTE register index, or `None` if this file does not
/// name it. The reverse of the [`context_reg`] / [`sh_reg`] constant tables (task-185).
///
/// Deliberately a reverse index over the constants above rather than a second, fuller GFX6
/// register table: the constants are the ones this emulator's draw path actually consumes,
/// so a name here means "we read this" and a `None` means "the guest wrote a register we do
/// not interpret". That distinction is the whole point — task-179 lost hours to three
/// registers the guest wrote and nothing read, which no probe was looking at. The snapshot
/// dumper therefore emits EVERY written register, naming the ones it can and printing the
/// raw index for the rest; it never skips an unnamed one.
///
/// The two indexed blocks (`SPI_PS_INPUT_CNTL_n`, `SPI_SHADER_USER_DATA_{VS,PS}_n`) are
/// matched by range and rendered with their slot number, so all 32 / 16 entries name
/// themselves without 48 more constants.
pub fn reg_name(index: u32) -> Option<String> {
    use context_reg as ctx;
    use sh_reg as sh;

    // Indexed blocks first: a range match, not one constant per slot.
    if (ctx::SPI_PS_INPUT_CNTL_0..ctx::SPI_PS_INPUT_CNTL_0 + 32).contains(&index) {
        return Some(format!(
            "SPI_PS_INPUT_CNTL_{}",
            index - ctx::SPI_PS_INPUT_CNTL_0
        ));
    }
    if (sh::SPI_SHADER_USER_DATA_VS_0..sh::SPI_SHADER_USER_DATA_VS_0 + sh::USER_DATA_SLOTS)
        .contains(&index)
    {
        return Some(format!(
            "SPI_SHADER_USER_DATA_VS_{}",
            index - sh::SPI_SHADER_USER_DATA_VS_0
        ));
    }
    if (sh::SPI_SHADER_USER_DATA_PS_0..sh::SPI_SHADER_USER_DATA_PS_0 + sh::USER_DATA_SLOTS)
        .contains(&index)
    {
        return Some(format!(
            "SPI_SHADER_USER_DATA_PS_{}",
            index - sh::SPI_SHADER_USER_DATA_PS_0
        ));
    }

    // Scalar constants. Resolved from the [`reg::ALL_PIPELINE_REGS`] catalog so this reverse
    // index and the coverage audit share one source of truth: a constant that is not
    // catalogued cannot be named here, and a catalogued one is forced through the audit test.
    reg::ALL_PIPELINE_REGS
        .iter()
        .find(|(i, _)| *i == index)
        .map(|(_, name)| (*name).to_string())
}

/// The register window a SET_*_REG opcode writes into, or `None` for non-SET
/// opcodes. Lets the trace renderer resolve an absolute register index. Glue over the
/// already-defined [`op`] opcodes and [`reg_base`] windows: a PM4 `SET_<WINDOW>_REG`
/// packet's body offsets are relative to that window's base (Mesa `SI_*_REG_OFFSET`), so
/// SET_CONFIG_REG → CONFIG, SET_CONTEXT_REG → CONTEXT, SET_SH_REG → SH, SET_UCONFIG_REG → UCONFIG.
pub fn set_reg_base(opcode: u8) -> Option<u32> {
    use op::*;
    Some(match opcode {
        IT_SET_CONFIG_REG => reg_base::CONFIG,
        IT_SET_CONTEXT_REG => reg_base::CONTEXT,
        IT_SET_SH_REG => reg_base::SH,
        IT_SET_UCONFIG_REG => reg_base::UCONFIG,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_maps_known_opcode() {
        assert_eq!(name(op::IT_NOP), Some("IT_NOP"));
        assert_eq!(name(op::IT_DRAW_INDEX_AUTO), Some("IT_DRAW_INDEX_AUTO"));
        assert_eq!(name(op::IT_SET_CONTEXT_REG), Some("IT_SET_CONTEXT_REG"));
        assert_eq!(name(op::IT_DMA_DATA), Some("IT_DMA_DATA"));
    }

    #[test]
    fn name_unknown_opcode_falls_back_to_none() {
        // 0x00 and 0xFF are not assigned IT_* opcodes.
        assert_eq!(name(0x00), None);
        assert_eq!(name(0xFF), None);
        assert_eq!(name(0xEE), None);
    }

    #[test]
    fn reg_name_resolves_scalar_and_indexed_registers() {
        assert_eq!(
            reg_name(context_reg::CB_COLOR0_BASE).as_deref(),
            Some("CB_COLOR0_BASE")
        );
        assert_eq!(
            reg_name(sh_reg::SPI_SHADER_PGM_LO_VS).as_deref(),
            Some("SPI_SHADER_PGM_LO_VS")
        );
        // Indexed blocks name every slot, not just slot 0.
        assert_eq!(
            reg_name(context_reg::SPI_PS_INPUT_CNTL_0 + 7).as_deref(),
            Some("SPI_PS_INPUT_CNTL_7")
        );
        assert_eq!(
            reg_name(sh_reg::SPI_SHADER_USER_DATA_VS_0 + 15).as_deref(),
            Some("SPI_SHADER_USER_DATA_VS_15")
        );
        assert_eq!(
            reg_name(sh_reg::SPI_SHADER_USER_DATA_PS_0 + 4).as_deref(),
            Some("SPI_SHADER_USER_DATA_PS_4")
        );
        // One past the block is NOT part of it.
        assert_eq!(
            reg_name(sh_reg::SPI_SHADER_USER_DATA_PS_0 + sh_reg::USER_DATA_SLOTS).as_deref(),
            None
        );
    }

    /// A register this file does not name must return `None` so the caller falls back to the
    /// raw index — the snapshot dumper prints it either way. Skipping unnamed registers is
    /// exactly how task-179 lost three of the four registers that mattered.
    #[test]
    fn reg_name_unknown_register_is_none_not_a_guess() {
        assert_eq!(reg_name(reg_base::CONTEXT + 0x3FF), None);
        assert_eq!(reg_name(0), None);
    }

    #[test]
    fn set_reg_base_returns_matching_window() {
        assert_eq!(set_reg_base(op::IT_SET_CONFIG_REG), Some(reg_base::CONFIG));
        assert_eq!(
            set_reg_base(op::IT_SET_CONTEXT_REG),
            Some(reg_base::CONTEXT)
        );
        assert_eq!(set_reg_base(op::IT_SET_SH_REG), Some(reg_base::SH));
        assert_eq!(
            set_reg_base(op::IT_SET_UCONFIG_REG),
            Some(reg_base::UCONFIG)
        );
    }

    #[test]
    fn set_reg_base_returns_none_for_non_set_opcodes() {
        // A non-SET opcode (even a known one) has no register window.
        assert_eq!(set_reg_base(op::IT_NOP), None);
        assert_eq!(set_reg_base(op::IT_DRAW_INDEX_AUTO), None);
        assert_eq!(set_reg_base(0xFF), None);
    }

    #[test]
    fn reg_base_constants_match_gfx6_windows() {
        assert_eq!(reg_base::CONFIG, 0x2000);
        assert_eq!(reg_base::CONTEXT, 0xA000);
        assert_eq!(reg_base::SH, 0x2C00);
        assert_eq!(reg_base::UCONFIG, 0xC000);
    }

    /// Pins every PM4 Type-3 opcode, register-window base, and `PGM_RSRC` bitfield to
    /// its AMD hardware value. The right-hand literals are the constants in Mesa
    /// `src/amd/{common/sid.h, registers/gfx6.json}` (`PKT3_*`, `SI_*_REG_OFFSET`,
    /// register maps), corroborated by the Linux kernel
    /// `drivers/gpu/drm/radeon/{sid,cikd}.h` (`PACKET3_*`); this test fails if ours drift
    /// from those AMD definitions.
    #[test]
    fn pm4_opcodes_match_amd_oracle() {
        // (our const, AMD PACKET3_*/PKT3_* value).
        let oracle: [(u8, u8); 25] = [
            (op::IT_NOP, 0x10),
            (op::IT_CLEAR_STATE, 0x12),
            (op::IT_INDEX_BUFFER_SIZE, 0x13),
            (op::IT_DISPATCH_DIRECT, 0x15),
            (op::IT_DISPATCH_INDIRECT, 0x16),
            (op::IT_INDEX_BASE, 0x26),
            (op::IT_DRAW_INDEX_2, 0x27),
            (op::IT_CONTEXT_CONTROL, 0x28),
            (op::IT_INDEX_TYPE, 0x2A),
            (op::IT_DRAW_INDEX_AUTO, 0x2D),
            (op::IT_NUM_INSTANCES, 0x2F),
            (op::IT_DRAW_INDEX_OFFSET_2, 0x35),
            (op::IT_WRITE_DATA, 0x37),
            (op::IT_WAIT_REG_MEM, 0x3C),
            (op::IT_INDIRECT_BUFFER, 0x3F),
            (op::IT_PFP_SYNC_ME, 0x42),
            (op::IT_EVENT_WRITE, 0x46),
            (op::IT_EVENT_WRITE_EOP, 0x47),
            (op::IT_EVENT_WRITE_EOS, 0x48),
            (op::IT_DMA_DATA, 0x50),
            (op::IT_ACQUIRE_MEM, 0x58), // GFX7+
            (op::IT_SET_CONFIG_REG, 0x68),
            (op::IT_SET_CONTEXT_REG, 0x69),
            (op::IT_SET_SH_REG, 0x76),
            (op::IT_SET_UCONFIG_REG, 0x79), // GFX7+
        ];
        for (ours, amd) in oracle {
            assert_eq!(ours, amd, "PM4 opcode {ours:#04X} != AMD {amd:#04X}");
        }

        // Register-window bases = AMD `*_REG_OFFSET` byte offsets >> 2 (byte → dword):
        // CONFIG 0x8000, SH 0xB000, CONTEXT 0x28000, UCONFIG 0x30000 (CIK+).
        assert_eq!(reg_base::CONFIG, 0x8000 >> 2);
        assert_eq!(reg_base::SH, 0xB000 >> 2);
        assert_eq!(reg_base::CONTEXT, 0x28000 >> 2);
        assert_eq!(reg_base::UCONFIG, 0x30000 >> 2);

        // `PGM_RSRC1/2` bitfields = Mesa gfx6.json field positions: VGPRS [0:5],
        // SGPRS [6:9], USER_SGPR [1:5]. Drive each field to all-ones and check decode.
        assert_eq!(pgm_rsrc::num_vgprs(0x3F), (0x3F + 1) * 4);
        assert_eq!(pgm_rsrc::num_sgprs(0xF << 6), (0xF + 1) * 8);
        assert_eq!(pgm_rsrc::num_user_sgprs(0x1F << 1), 0x1F);

        // Type-3 header field positions = Mesa `PKT3`: PKT_TYPE_S(3)<<30,
        // PKT_COUNT_S(count)<<16, PKT3_IT_OPCODE_S(op)<<8, count = body_len - 1.
        let h = t3_header(0x37, 3);
        assert_eq!(h >> 30, 0b11); // type in [31:30]
        assert_eq!((h >> 16) & 0x3FFF, 3 - 1); // count = body_len-1 in [29:16]
        assert_eq!((h >> 8) & 0xFF, 0x37); // opcode in [15:8]
    }

    /// Drift guard: `opcodes.rs` is the single source of truth for the PM4
    /// `IT_*` opcodes and GFX6 register-window bases. The guest PM4 test corpus
    /// (`examples/ps4-pm4-test/ps4-pm4-test/main.c`) re-`#define`s the handful it
    /// hand-emits, with a "mirror opcodes.rs" comment. Nothing else enforces
    /// that mirror: a silent divergence would still compile and run, tracing the
    /// wrong packet / mis-resolving a register window. This test parses the
    /// corpus at test time and asserts every shared constant matches, so an edit
    /// to *either* side that diverges fails `cargo test`.
    #[test]
    fn corpus_mirror_matches_opcodes() {
        // Path relative to this crate (crates/gnm) up to the repo-root example.
        let corpus_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/ps4-pm4-test/ps4-pm4-test/main.c"
        );
        let src = std::fs::read_to_string(corpus_path)
            .unwrap_or_else(|e| panic!("read PM4 corpus {corpus_path}: {e}"));

        // Parse `#define <NAME> 0x<hex>` (ignoring trailing `//` comments).
        let defines: std::collections::HashMap<&str, u32> = src
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("#define ")?;
                let mut it = rest.split_whitespace();
                let name = it.next()?;
                let val = it.next()?;
                let hex = val.strip_prefix("0x").or_else(|| val.strip_prefix("0X"))?;
                let n = u32::from_str_radix(hex, 16).ok()?;
                Some((name, n))
            })
            .collect();

        // `#define` name -> expected value in `pm4::opcodes` (source of truth).
        // Only the constants the corpus actually shares are guarded here.
        let expected: [(&str, u32); 5] = [
            ("IT_CLEAR_STATE", op::IT_CLEAR_STATE as u32),
            ("IT_CONTEXT_CONTROL", op::IT_CONTEXT_CONTROL as u32),
            ("IT_DRAW_INDEX_AUTO", op::IT_DRAW_INDEX_AUTO as u32),
            ("IT_SET_CONTEXT_REG", op::IT_SET_CONTEXT_REG as u32),
            ("IT_SET_SH_REG", op::IT_SET_SH_REG as u32),
        ];
        for (name, want) in expected {
            let got = defines.get(name).unwrap_or_else(|| {
                panic!(
                    "PM4 corpus {corpus_path} no longer `#define`s {name}; \
                     update this drift guard if the shared surface changed"
                )
            });
            assert_eq!(
                *got, want,
                "PM4 opcode drift: corpus #define {name} = {got:#04X} but \
                 opcodes.rs (source of truth) op::{name} = {want:#04X}"
            );
        }

        // The corpus doesn't `#define` the register-window bases; it references
        // them in a comment ("CONTEXT base 0xA000, SH base 0x2C00"). Guard that
        // wording so a base change in `reg_base` is reflected in the corpus doc.
        for (label, base) in [("CONTEXT", reg_base::CONTEXT), ("SH", reg_base::SH)] {
            let needle = format!("{label} base {base:#06X}");
            let needle_lower = format!("{label} base {base:#06x}");
            assert!(
                src.contains(&needle) || src.contains(&needle_lower),
                "PM4 reg-base drift: corpus comment should say \"{needle}\" to \
                 match reg_base::{label} (source of truth); update whichever side \
                 is stale"
            );
        }
    }

    // ---- task-183: GFX6 pipeline-affecting register coverage audit ----
    //
    // These two lists classify every register in [`reg::ALL_PIPELINE_REGS`]: either the
    // GNM→pipeline derivation CONSUMES it (`READ_BY_DERIVATION`) or it carries a cited reason
    // it is safe to ignore (`IGNORED_WITH_REASON`). The audit doc is
    // `backlog/docs/doc-8 - GFX6-pipeline-affecting-register-audit-—-derivation-coverage.md`.
    // Hardware facts are the AMD GFX6 (Liverpool) definitions in Mesa
    // `src/amd/registers/gfx6.json`; occupancy semantics are the AMD GCN ISA
    // `SPI_SHADER_PGM_RSRC3_*` sections.

    /// Registers a value fetched from the shadow register file is consumed to build
    /// pipeline/draw state (derive.rs target/pipeline/viewport/scissor; state.rs shader
    /// identity + resources + PS routing; vbuf.rs/exec.rs user-data descriptor pull). A read
    /// that only feeds a diagnostic probe or only emits the register does NOT count here.
    const READ_BY_DERIVATION: &[u32] = &[
        sh_reg::SPI_SHADER_PGM_LO_PS,
        sh_reg::SPI_SHADER_PGM_HI_PS,
        sh_reg::SPI_SHADER_PGM_RSRC1_PS,
        sh_reg::SPI_SHADER_PGM_RSRC2_PS,
        sh_reg::SPI_SHADER_PGM_LO_VS,
        sh_reg::SPI_SHADER_PGM_HI_VS,
        sh_reg::SPI_SHADER_PGM_RSRC1_VS,
        sh_reg::SPI_SHADER_PGM_RSRC2_VS,
        sh_reg::SPI_SHADER_USER_DATA_PS_0,
        sh_reg::SPI_SHADER_USER_DATA_VS_0,
        context_reg::SPI_PS_INPUT_CNTL_0,
        context_reg::CB_TARGET_MASK,
        context_reg::CB_COLOR0_BASE,
        context_reg::CB_COLOR0_PITCH,
        context_reg::CB_COLOR0_SLICE,
        context_reg::CB_COLOR0_INFO,
        context_reg::CB_COLOR0_ATTRIB,
        context_reg::PA_CL_VPORT_XSCALE,
        context_reg::PA_CL_VPORT_XOFFSET,
        context_reg::PA_CL_VPORT_YSCALE,
        context_reg::PA_CL_VPORT_YOFFSET,
        context_reg::PA_SC_SCREEN_SCISSOR_TL,
        context_reg::PA_SC_SCREEN_SCISSOR_BR,
        context_reg::CB_BLEND0_CONTROL,
        context_reg::DB_DEPTH_CONTROL,
        context_reg::DB_Z_INFO,
        uconfig::VGT_PRIMITIVE_TYPE,
    ];

    /// Registers the derivation does NOT consume, each with a short cited reason it is safe to
    /// ignore. Entries tagged `GAP task-N` are genuinely pipeline-affecting and tracked by a
    /// follow-up task (the audit deliberately does not model them — CLAUDE.md scope: audit =
    /// audit). Reasons are forward-only and cite the clean source by name.
    const IGNORED_WITH_REASON: &[(u32, &str)] = &[
        (
            sh_reg::SPI_SHADER_PGM_RSRC3_PS,
            "CU-enable/wave-limit occupancy mask; scheduling hint with no effect on rendered \
             output (GCN ISA SPI_SHADER_PGM_RSRC3_PS)",
        ),
        (
            sh_reg::SPI_SHADER_PGM_RSRC3_VS,
            "CU-enable/wave-limit occupancy mask; scheduling hint with no effect on rendered \
             output (GCN ISA SPI_SHADER_PGM_RSRC3_VS)",
        ),
        (
            context_reg::SPI_VS_OUT_CONFIG,
            "VS export-param count; the recompiler derives the export layout from the shader \
             binary (Mesa gfx6.json SPI_VS_OUT_CONFIG)",
        ),
        (
            context_reg::SPI_SHADER_POS_FORMAT,
            "VS position-export format; the recompiler emits gl_Position directly (Mesa \
             gfx6.json SPI_SHADER_POS_FORMAT)",
        ),
        (
            context_reg::PA_CL_VS_OUT_CNTL,
            "Clip/cull-distance output enables; the software raster path applies no user \
             clip/cull planes (Mesa gfx6.json PA_CL_VS_OUT_CNTL)",
        ),
        (
            context_reg::CB_SHADER_MASK,
            "PS output-component mask; redundant with CB_TARGET_MASK (read) on the single-MRT \
             RGBA8 path (Mesa gfx6.json CB_SHADER_MASK)",
        ),
        (
            context_reg::CB_COLOR_CONTROL,
            "Global CB mode/ROP; current titles program a normal blend mode and ROP=copy, so \
             the diagnostic-only read suffices (Mesa gfx6.json CB_COLOR_CONTROL)",
        ),
        (
            context_reg::CB_COLOR0_VIEW,
            "MRT0 array-slice range; current targets are single-slice 2D surfaces \
             (SLICE_START=0), so no array-slice selection applies (Mesa gfx6.json \
             CB_COLOR0_VIEW)",
        ),
        (
            context_reg::SPI_PS_INPUT_ENA,
            "GAP task-234: PS interpolant enable mask; derivation routes PS inputs via \
             SPI_PS_INPUT_CNTL only (Mesa gfx6.json SPI_PS_INPUT_ENA)",
        ),
        (
            context_reg::SPI_PS_INPUT_ADDR,
            "GAP task-234: PS interpolant address mask; derivation routes PS inputs via \
             SPI_PS_INPUT_CNTL only (Mesa gfx6.json SPI_PS_INPUT_ADDR)",
        ),
        (
            context_reg::SPI_PS_IN_CONTROL,
            "GAP task-234: PS input count (NUM_INTERP); not consumed by PS input derivation \
             (Mesa gfx6.json SPI_PS_IN_CONTROL)",
        ),
        (
            context_reg::SPI_BARYC_CNTL,
            "GAP task-234: barycentric/interpolation mode; not modelled by the recompiler \
             (Mesa gfx6.json SPI_BARYC_CNTL)",
        ),
        (
            context_reg::SPI_SHADER_COL_FORMAT,
            "GAP task-235: PS colour-export numeric format per MRT; colour format is taken \
             from CB_COLOR0_INFO on the RGBA8 path (Mesa gfx6.json SPI_SHADER_COL_FORMAT)",
        ),
        (
            context_reg::SPI_SHADER_Z_FORMAT,
            "GAP task-235: PS depth-export format; PS depth export is not yet modelled (Mesa \
             gfx6.json SPI_SHADER_Z_FORMAT)",
        ),
        (
            context_reg::DB_SHADER_CONTROL,
            "GAP task-235: Z-export/kill/mask control; PS discard and depth export are not yet \
             modelled (Mesa gfx6.json DB_SHADER_CONTROL)",
        ),
    ];

    /// AC#3 anti-drift guard (task-183): every register in [`reg::ALL_PIPELINE_REGS`] must be
    /// classified EITHER as consumed by the derivation (`READ_BY_DERIVATION`) OR with a cited
    /// reason it is safe to ignore (`IGNORED_WITH_REASON`). A register added to the catalog but
    /// left unclassified fails this test — so a newly-added-but-unread register (the "guest
    /// wrote it, nothing read it" bug class, task-179) cannot silently slip through: the
    /// maintainer must model it in the derivation or add a reasoned ignore entry.
    #[test]
    fn pipeline_register_coverage_is_audited() {
        use std::collections::HashSet;

        let read: HashSet<u32> = READ_BY_DERIVATION.iter().copied().collect();
        assert_eq!(
            read.len(),
            READ_BY_DERIVATION.len(),
            "READ_BY_DERIVATION has a duplicate register index"
        );
        let ignored: HashSet<u32> = IGNORED_WITH_REASON.iter().map(|(i, _)| *i).collect();
        assert_eq!(
            ignored.len(),
            IGNORED_WITH_REASON.len(),
            "IGNORED_WITH_REASON has a duplicate register index"
        );

        // A register is either consumed or ignored-with-reason — never both.
        for idx in &read {
            assert!(
                !ignored.contains(idx),
                "register {idx:#06X} is in BOTH READ_BY_DERIVATION and IGNORED_WITH_REASON; \
                 it must be exactly one"
            );
        }

        // Every ignore reason is non-empty and names its register (a cited, legible reason —
        // not a bare entry that re-opens the blind spot).
        for (idx, reason) in IGNORED_WITH_REASON {
            let name = reg_name(*idx)
                .unwrap_or_else(|| panic!("ignored register {idx:#06X} is not in the catalog"));
            assert!(
                reason.contains(name.as_str()),
                "IGNORED_WITH_REASON entry for {name} ({idx:#06X}) must cite the register by \
                 name in its reason; got {reason:?}"
            );
        }

        // The load-bearing partition check: every catalogued register is classified exactly
        // once. The failure message tells the maintainer precisely what to do.
        for (idx, name) in reg::ALL_PIPELINE_REGS {
            let is_read = read.contains(idx);
            let is_ignored = ignored.contains(idx);
            assert!(
                is_read ^ is_ignored,
                "register {name} ({idx:#06X}) is not audited: it is in {}. Model it in the \
                 derivation and add it to READ_BY_DERIVATION, or add a cited entry to \
                 IGNORED_WITH_REASON (see doc-8 GFX6 register audit).",
                if is_read && is_ignored {
                    "BOTH coverage lists"
                } else {
                    "NEITHER coverage list"
                }
            );

            // The catalog name must round-trip through reg_name (ties the audit to the live
            // register constants).
            assert_eq!(
                reg_name(*idx).as_deref(),
                Some(*name),
                "reg_name({idx:#06X}) disagrees with the catalog name {name}"
            );
        }

        // Both lists cover only catalogued registers (no stale entry for a removed constant).
        let catalog: HashSet<u32> = reg::ALL_PIPELINE_REGS.iter().map(|(i, _)| *i).collect();
        for idx in read.iter().chain(ignored.iter()) {
            assert!(
                catalog.contains(idx),
                "register {idx:#06X} is classified but not in reg::ALL_PIPELINE_REGS \
                 (stale entry — remove it or add the constant to the catalog)"
            );
        }

        // Sanity on the audited totals, so an accidental list edit that still partitions is
        // still visible in review (the audit doc quotes these figures).
        assert_eq!(
            reg::ALL_PIPELINE_REGS.len(),
            42,
            "catalogued register count"
        );
        assert_eq!(READ_BY_DERIVATION.len(), 27, "registers read by derivation");
        assert_eq!(
            IGNORED_WITH_REASON.len(),
            15,
            "registers ignored with reason"
        );
    }
}
