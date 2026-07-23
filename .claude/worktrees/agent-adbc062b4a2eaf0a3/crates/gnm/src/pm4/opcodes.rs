//! PM4 opcode tables / `IT_*` constants and register-base constants
//! (doc-4 §1, §5). Vulkan-free, no execution.
//!
//! Opcode values mirror shadPS4's `video_core/amdgpu/pm4_opcodes.h` (the AMD
//! GFX6/PM4 IT_* enumeration). The register-base constants are the standard
//! GFX6 CP register windows the SET_*_REG packets add their per-entry offset to.
//!
//! **This file is the single source of truth for the shared PM4 `IT_*` opcodes
//! and GFX6 register-window bases.** The guest PM4 test corpus
//! (`examples/ps4-pm4-test/ps4-pm4-test/main.c`) re-`#define`s a handful of
//! these so it can hand-emit named packets — that file mirrors these values, it
//! does not own them. The `corpus_mirror_matches_opcodes` test below reads the
//! corpus at test time and fails if either side drifts.

/// PM4 Type-3 IT_* opcodes (bits [15:8] of a Type-3 header). Values are exact
/// AMD/GFX6 opcodes as used by shadPS4.
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
/// (GFX6 `SPI_SHADER_*`, dword indices). These are the standard AMD GFX6 offsets
/// (SH window base [`reg_base::SH`] + the per-stage sub-offsets the guest PM4 test
/// corpus mirrors as `SH_SPI_SHADER_PGM_LO_VS = 0x48` / `_PS = 0x08` in
/// `examples/ps4-pm4-test/ps4-pm4-test/main.c`, matching shadPS4
/// `video_core/amdgpu/*`). The draw path reads `PGM_LO/HI` to derive the `.sb` code
/// address (doc-4 §5) and `PGM_RSRC1/2` for the GPR / user-SGPR counts.
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
    /// `R_00B01C` in Mesa `src/amd/common/sid.h` → SH dword `0x1C/4 = 0x07`.
    pub const SPI_SHADER_PGM_RSRC3_PS: u32 = SH + 0x07;
    /// VS `SPI_SHADER_PGM_RSRC3_VS` — CU-enable / wave-limit mask. SH byte address
    /// `R_00B118` in Mesa `sid.h` → SH dword `0x118/4 = 0x46`.
    pub const SPI_SHADER_PGM_RSRC3_VS: u32 = SH + 0x46;

    /// Slot 0 of the 16-entry `SPI_SHADER_USER_DATA_PS_*` block — the user-SGPR words
    /// the driver preloads before the PS runs (V#/T#/S# pointers, inline constants).
    /// SH byte address `R_00B030` in Mesa `sid.h` → SH dword `0x30/4 = 0x0C`; slot `i`
    /// is `SPI_SHADER_USER_DATA_PS_0 + i`, matching the per-stage `PGM_LO` offset + 4.
    pub const SPI_SHADER_USER_DATA_PS_0: u32 = SH + 0x0C;
    /// Slot 0 of the 16-entry `SPI_SHADER_USER_DATA_VS_*` block. SH byte address
    /// `R_00B130` in Mesa `sid.h` → SH dword `0x130/4 = 0x4C`; slot `i` is
    /// `SPI_SHADER_USER_DATA_VS_0 + i`.
    pub const SPI_SHADER_USER_DATA_VS_0: u32 = SH + 0x4C;

    /// Number of user-SGPR slots in a `SPI_SHADER_USER_DATA_*` block (GFX6: 16).
    pub const USER_DATA_SLOTS: u32 = 16;
}

/// Absolute CONTEXT-bank register indices for the graphics VS/PS pipeline state that
/// the Gnm `VsStageRegisters` / `PsStageRegisters` structs carry alongside the SH
/// shader-program run. Offsets are the standard AMD SI/GFX6 context registers: the
/// dword offset is `(R_ byte address - 0x28000) / 4`, where the `R_02xxxx` byte
/// addresses are Mesa's `src/amd/common/sid.h` (radeonsi) definitions. Corroborated
/// by the in-repo corpus, whose `CONTEXT_SPI_SHADER_COL_FORMAT 0x01C5`
/// (`examples/ps4-pm4-test/ps4-pm4-test/main.c`) matches `SPI_SHADER_COL_FORMAT`
/// below.
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

    /// `PA_SC_SCREEN_SCISSOR_TL` — screen scissor top-left (x[15:0], y[31:16]), `R_028034`.
    pub const PA_SC_SCREEN_SCISSOR_TL: u32 = CONTEXT + 0x00D;
    /// `PA_SC_SCREEN_SCISSOR_BR` — screen scissor bottom-right (x[15:0], y[31:16]), `R_028038`.
    pub const PA_SC_SCREEN_SCISSOR_BR: u32 = CONTEXT + 0x00E;

    /// `CB_BLEND0_CONTROL` — MRT0 blend equation / factors (`R_028780`).
    pub const CB_BLEND0_CONTROL: u32 = CONTEXT + 0x1E0;
    /// `CB_COLOR_CONTROL` — global color-buffer mode / ROP (`R_028808`).
    pub const CB_COLOR_CONTROL: u32 = CONTEXT + 0x202;

    /// `DB_DEPTH_CONTROL` — depth-test enable / compare / stencil (`R_028800`).
    pub const DB_DEPTH_CONTROL: u32 = CONTEXT + 0x200;
    /// `DB_Z_INFO` — depth-surface format / tiling (`R_028040`).
    pub const DB_Z_INFO: u32 = CONTEXT + 0x010;
}

/// Decoders for the GFX6 `SPI_SHADER_PGM_RSRC1/2` bitfields (GPR / user-SGPR
/// counts). These are the register-truth inputs the draw path snapshots into a
/// shader's resource footprint (doc-4 §5). Encodings are the standard AMD GFX6
/// layout (mirrored from shadPS4 `video_core/amdgpu/liverpool.h`):
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
/// dword length" convention). `body_len` is the number of dwords that follow the
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

/// The register window a SET_*_REG opcode writes into, or `None` for non-SET
/// opcodes. Lets the trace renderer resolve an absolute register index.
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
}
