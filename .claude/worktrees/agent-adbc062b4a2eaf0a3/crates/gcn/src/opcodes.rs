//! GCN opcode → mnemonic tables for the SI/CI (GFX6/GFX7) encodings the corpus
//! subset covers (doc-4 §1, phase 4).
//!
//! Values are the AMD Southern Islands / Sea Islands ISA opcodes, cross-referenced
//! against the shadPS4 / GPCS4 GCN decoders. Only the mnemonics the corpus exercises
//! (plus a few obvious neighbours) are named; an unmapped opcode disassembles to a
//! numeric form (`op<class>_<n>`) — the decode still succeeds, only the pretty name
//! is missing. This is the single source of truth for the shared names, mirroring
//! the `pm4::opcodes` discipline.
//!
//! Op-field verification: every VOP1/VOP2/VOP3 opcode number here was checked
//! authoritatively against `llvm-mc` for the GFX7 / Sea Islands target. Reproduce
//! with, e.g.:
//!
//! ```sh
//! echo 'v_sqrt_f32 v0, v1' | llvm-mc -triple amdgcn -mcpu=bonaire -show-encoding
//! ```
//!
//! and extract the op field from the emitted 4-byte encoding (VOP1: bits[16:9] of
//! the first dword; VOP2: bits[30:25]; VOP3: bits[25:17]). llvm-mc is the authority
//! — the GFX7 numbering differs from GFX8/VI (e.g. `v_mad_f32` is 0x141 on GFX7 but
//! 0x1C1 on VI), so numbers are pinned to the bonaire disassembler here.

/// SOP1 (scalar, one input) opcodes.
pub mod sop1 {
    pub const S_MOV_B32: u8 = 0x03;
    pub const S_MOV_B64: u8 = 0x04;
    pub const S_WQM_B64: u8 = 0x08;
    /// `s_setpc_b64` — branch to the 64-bit address in `ssrc0` (the fetch-shader
    /// return). GFX7 op 0x20 (verified against llvm-mc bonaire).
    pub const S_SETPC_B64: u8 = 0x20;
    /// `s_swappc_b64` — save PC to `sdst`, branch to `ssrc0` (subroutine call).
    /// GFX7 op 0x21 (verified against llvm-mc bonaire).
    pub const S_SWAPPC_B64: u8 = 0x21;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            S_MOV_B32 => "s_mov_b32",
            S_MOV_B64 => "s_mov_b64",
            S_WQM_B64 => "s_wqm_b64",
            S_SETPC_B64 => "s_setpc_b64",
            S_SWAPPC_B64 => "s_swappc_b64",
            _ => return None,
        })
    }
}

/// SOP2 (scalar, two inputs) opcodes.
pub mod sop2 {
    pub const S_ADD_U32: u8 = 0x00;
    pub const S_SUB_U32: u8 = 0x01;
    pub const S_ADD_I32: u8 = 0x02;
    pub const S_AND_B32: u8 = 0x0E;
    pub const S_OR_B32: u8 = 0x10;
    pub const S_LSHL_B32: u8 = 0x1E;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            S_ADD_U32 => "s_add_u32",
            S_SUB_U32 => "s_sub_u32",
            S_ADD_I32 => "s_add_i32",
            S_AND_B32 => "s_and_b32",
            S_OR_B32 => "s_or_b32",
            S_LSHL_B32 => "s_lshl_b32",
            _ => return None,
        })
    }
}

/// SOPK (scalar + 16-bit immediate) opcodes.
pub mod sopk {
    pub const S_MOVK_I32: u8 = 0x00;
    pub const S_CMPK_EQ_I32: u8 = 0x03;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            S_MOVK_I32 => "s_movk_i32",
            S_CMPK_EQ_I32 => "s_cmpk_eq_i32",
            _ => return None,
        })
    }
}

/// SOPC (scalar compare) opcodes.
pub mod sopc {
    pub const S_CMP_EQ_I32: u8 = 0x00;
    pub const S_CMP_LG_I32: u8 = 0x01;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            S_CMP_EQ_I32 => "s_cmp_eq_i32",
            S_CMP_LG_I32 => "s_cmp_lg_i32",
            _ => return None,
        })
    }
}

/// SOPP (scalar program control) opcodes.
pub mod sopp {
    pub const S_NOP: u8 = 0x00;
    pub const S_ENDPGM: u8 = 0x01;
    pub const S_BRANCH: u8 = 0x02;
    pub const S_CBRANCH_SCC0: u8 = 0x04;
    pub const S_CBRANCH_SCC1: u8 = 0x05;
    pub const S_CBRANCH_VCCZ: u8 = 0x06;
    pub const S_CBRANCH_VCCNZ: u8 = 0x07;
    pub const S_CBRANCH_EXECZ: u8 = 0x08;
    pub const S_CBRANCH_EXECNZ: u8 = 0x09;
    pub const S_BARRIER: u8 = 0x0A;
    pub const S_WAITCNT: u8 = 0x0C;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            S_NOP => "s_nop",
            S_ENDPGM => "s_endpgm",
            S_BRANCH => "s_branch",
            S_CBRANCH_SCC0 => "s_cbranch_scc0",
            S_CBRANCH_SCC1 => "s_cbranch_scc1",
            S_CBRANCH_VCCZ => "s_cbranch_vccz",
            S_CBRANCH_VCCNZ => "s_cbranch_vccnz",
            S_CBRANCH_EXECZ => "s_cbranch_execz",
            S_CBRANCH_EXECNZ => "s_cbranch_execnz",
            S_BARRIER => "s_barrier",
            S_WAITCNT => "s_waitcnt",
            _ => return None,
        })
    }
}

/// SMRD (scalar memory read) opcodes.
pub mod smrd {
    pub const S_LOAD_DWORD: u8 = 0x00;
    pub const S_LOAD_DWORDX2: u8 = 0x01;
    pub const S_LOAD_DWORDX4: u8 = 0x02;
    pub const S_LOAD_DWORDX8: u8 = 0x03;
    pub const S_LOAD_DWORDX16: u8 = 0x04;
    pub const S_BUFFER_LOAD_DWORD: u8 = 0x08;
    pub const S_BUFFER_LOAD_DWORDX4: u8 = 0x0A;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            S_LOAD_DWORD => "s_load_dword",
            S_LOAD_DWORDX2 => "s_load_dwordx2",
            S_LOAD_DWORDX4 => "s_load_dwordx4",
            S_LOAD_DWORDX8 => "s_load_dwordx8",
            S_LOAD_DWORDX16 => "s_load_dwordx16",
            S_BUFFER_LOAD_DWORD => "s_buffer_load_dword",
            S_BUFFER_LOAD_DWORDX4 => "s_buffer_load_dwordx4",
            _ => return None,
        })
    }

    /// Number of consecutive SGPR destinations the load writes (for `s[lo:hi]`
    /// disassembly). `None` for unmapped ops.
    pub fn dst_count(op: u8) -> Option<u8> {
        Some(match op {
            S_LOAD_DWORD | S_BUFFER_LOAD_DWORD => 1,
            S_LOAD_DWORDX2 => 2,
            S_LOAD_DWORDX4 | S_BUFFER_LOAD_DWORDX4 => 4,
            S_LOAD_DWORDX8 => 8,
            S_LOAD_DWORDX16 => 16,
            _ => return None,
        })
    }
}

/// VOP1 (vector, one input) opcodes.
pub mod vop1 {
    pub const V_NOP: u8 = 0x00;
    pub const V_MOV_B32: u8 = 0x01;
    pub const V_CVT_F32_I32: u8 = 0x05;
    pub const V_CVT_F32_U32: u8 = 0x06;
    pub const V_CVT_U32_F32: u8 = 0x07;
    pub const V_CVT_I32_F32: u8 = 0x08;
    pub const V_RCP_F32: u8 = 0x2A;
    // GFX7/bonaire op field (llvm-mc: `v_sqrt_f32` → encoding bits[16:9] == 0x33).
    pub const V_SQRT_F32: u8 = 0x33;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            V_NOP => "v_nop",
            V_MOV_B32 => "v_mov_b32",
            V_CVT_F32_I32 => "v_cvt_f32_i32",
            V_CVT_F32_U32 => "v_cvt_f32_u32",
            V_CVT_U32_F32 => "v_cvt_u32_f32",
            V_CVT_I32_F32 => "v_cvt_i32_f32",
            V_RCP_F32 => "v_rcp_f32",
            V_SQRT_F32 => "v_sqrt_f32",
            _ => return None,
        })
    }
}

/// VOP2 (vector, two inputs) opcodes.
pub mod vop2 {
    pub const V_ADD_F32: u8 = 0x03;
    pub const V_SUB_F32: u8 = 0x04;
    pub const V_MUL_F32: u8 = 0x08;
    pub const V_MAC_F32: u8 = 0x1F;
    pub const V_MADMK_F32: u8 = 0x20;
    pub const V_MADAK_F32: u8 = 0x21;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            V_ADD_F32 => "v_add_f32",
            V_SUB_F32 => "v_sub_f32",
            V_MUL_F32 => "v_mul_f32",
            V_MAC_F32 => "v_mac_f32",
            V_MADMK_F32 => "v_madmk_f32",
            V_MADAK_F32 => "v_madak_f32",
            _ => return None,
        })
    }

    /// VOP2 ops that carry a trailing 32-bit literal K as a second dword regardless
    /// of the src0 field (v_madmk/v_madak). These always consume an extra dword.
    pub fn has_inline_literal(op: u8) -> bool {
        matches!(op, V_MADMK_F32 | V_MADAK_F32)
    }
}

/// VOP3 (vector, three inputs) opcodes. VOP3 addresses VOP1/2/C ops too, offset by
/// the standard SI ranges: VOPC → 0x000, VOP2 → 0x100, VOP1 → 0x140, native VOP3 ≥
/// 0x150. Only native ops are named here; re-encoded VOP1/2/C ops render numerically.
pub mod vop3 {
    pub const V_MAD_F32: u16 = 0x141;
    pub const V_FMA_F32: u16 = 0x14B;
    pub const V_MED3_F32: u16 = 0x157;

    pub fn name(op: u16) -> Option<&'static str> {
        Some(match op {
            V_MAD_F32 => "v_mad_f32",
            V_FMA_F32 => "v_fma_f32",
            V_MED3_F32 => "v_med3_f32",
            _ => return None,
        })
    }
}

/// MIMG (image memory) opcodes. Only the sampling ops the corpus exercises are named;
/// the op field is bits[24:18] of the first dword (verified against llvm-mc GFX7:
/// `image_sample` → op 0x20).
pub mod mimg {
    pub const IMAGE_LOAD: u8 = 0x00;
    pub const IMAGE_STORE: u8 = 0x08;
    pub const IMAGE_SAMPLE: u8 = 0x20;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            IMAGE_LOAD => "image_load",
            IMAGE_STORE => "image_store",
            IMAGE_SAMPLE => "image_sample",
            _ => return None,
        })
    }
}

/// VINTRP (parameter interpolation) opcodes.
pub mod vintrp {
    pub const V_INTERP_P1_F32: u8 = 0x00;
    pub const V_INTERP_P2_F32: u8 = 0x01;
    pub const V_INTERP_MOV_F32: u8 = 0x02;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            V_INTERP_P1_F32 => "v_interp_p1_f32",
            V_INTERP_P2_F32 => "v_interp_p2_f32",
            V_INTERP_MOV_F32 => "v_interp_mov_f32",
            _ => return None,
        })
    }
}

/// MUBUF (untyped buffer) opcodes.
pub mod mubuf {
    pub const BUFFER_LOAD_FORMAT_X: u8 = 0x00;
    pub const BUFFER_LOAD_FORMAT_XY: u8 = 0x01;
    pub const BUFFER_LOAD_FORMAT_XYZ: u8 = 0x02;
    pub const BUFFER_LOAD_FORMAT_XYZW: u8 = 0x03;
    pub const BUFFER_STORE_FORMAT_X: u8 = 0x04;
    pub const BUFFER_LOAD_DWORD: u8 = 0x0C;
    pub const BUFFER_STORE_DWORD: u8 = 0x1C;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            BUFFER_LOAD_FORMAT_X => "buffer_load_format_x",
            BUFFER_LOAD_FORMAT_XY => "buffer_load_format_xy",
            BUFFER_LOAD_FORMAT_XYZ => "buffer_load_format_xyz",
            BUFFER_LOAD_FORMAT_XYZW => "buffer_load_format_xyzw",
            BUFFER_STORE_FORMAT_X => "buffer_store_format_x",
            BUFFER_LOAD_DWORD => "buffer_load_dword",
            BUFFER_STORE_DWORD => "buffer_store_dword",
            _ => return None,
        })
    }

    /// Number of consecutive VGPR data registers the format op reads/writes (for
    /// `v[lo:hi]` disassembly). `None` for unmapped ops.
    pub fn vdata_count(op: u8) -> Option<u8> {
        Some(match op {
            BUFFER_LOAD_FORMAT_X
            | BUFFER_STORE_FORMAT_X
            | BUFFER_LOAD_DWORD
            | BUFFER_STORE_DWORD => 1,
            BUFFER_LOAD_FORMAT_XY => 2,
            BUFFER_LOAD_FORMAT_XYZ => 3,
            BUFFER_LOAD_FORMAT_XYZW => 4,
            _ => return None,
        })
    }
}
