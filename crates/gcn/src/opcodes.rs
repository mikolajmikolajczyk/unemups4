//! GCN opcode → mnemonic tables for the SI/CI (GFX6/GFX7) encodings the corpus
//! subset covers (doc-2 §1, phase 4).
//!
//! Values are the AMD Sea Islands (GCN2 / GFX7 = PS4 Liverpool) ISA opcodes. Each
//! per-class op number is the microcode opcode published in the AMD Sea Islands ISA
//! (`oracles/amd/ci-isa.pdf`, per-format "Microcode <FMT> Opcode N"
//! instruction tables — e.g. "Microcode VOP1 Opcode 51 (0x33)" for `V_SQRT_F32`) and
//! is pinned to the `llvm-mc` GFX7 encoding by the `gcn_opcodes_match_amd_oracle` test
//! below. Only the mnemonics the corpus exercises (plus a few obvious neighbours) are
//! named; an unmapped opcode disassembles to a numeric form (`op<class>_<n>`) — the
//! decode still succeeds, only the pretty name is missing. This is the single source
//! of truth for the shared names, mirroring the `pm4::opcodes` discipline.
//!
//! Op-field encoding witness: every opcode number here is reproduced by assembling the
//! mnemonic with `llvm-mc` for the GFX7 / Sea Islands target (`gfx700`, a.k.a. bonaire)
//! and extracting the op field from the emitted encoding bytes:
//!
//! ```sh
//! echo 'v_sqrt_f32 v0, v1' | llvm-mc --assemble --arch=amdgcn --mcpu=gfx700 --show-encoding
//! # => encoding: [0x01,0x67,0x00,0x7e]; VOP1 op = bits[16:9] = byte1 0x67 >> 1 = 0x33
//! ```
//!
//! Op-field bit ranges per class (little-endian encoding bytes b0..b3): SOP1 [15:8]
//! = b1; SOP2 [29:23]; SOPK [27:23]; SOPC/SOPP [22:16] = b2 & 0x7F; SMRD [26:22];
//! VOPC [24:17] = ((b3 & 1) << 7) | (b2 >> 1); VOP1 [16:9] = b1 >> 1; VOP2 [30:25]
//! = b3 >> 1; VOP3 [25:17] = ((b3 & 3) << 7) | (b2 >> 1); MIMG/MUBUF [24:18];
//! VINTRP [17:16]. GFX7 numbering differs from GFX8/VI (e.g. `v_mad_f32` is 0x141 on
//! GFX7 but 0x1C1 on VI), so numbers are pinned to the GFX7 target here.

/// SOP1 (scalar, one input) opcodes.
pub mod sop1 {
    pub const S_MOV_B32: u8 = 0x03;
    pub const S_MOV_B64: u8 = 0x04;
    pub const S_WQM_B64: u8 = 0x0A;
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
    pub const S_BUFFER_LOAD_DWORDX2: u8 = 0x09;
    pub const S_BUFFER_LOAD_DWORDX4: u8 = 0x0A;
    pub const S_BUFFER_LOAD_DWORDX8: u8 = 0x0B;
    /// `s_buffer_load_dwordx16` — load 16 scalar dwords from a constant buffer (a
    /// 4×4 matrix, typically). GFX7 op 0x0C (verified against llvm-mc bonaire:
    /// `s_buffer_load_dwordx16 s[0:15], s[4:7], 0x0` → `[0x00,0x05,0x00,0xc3]`, so
    /// the op field byte3 0xc3 gives op 0x0C). The retail Celeste VS load their
    /// transform matrix with this right after the fetch-shader call.
    pub const S_BUFFER_LOAD_DWORDX16: u8 = 0x0C;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            S_LOAD_DWORD => "s_load_dword",
            S_LOAD_DWORDX2 => "s_load_dwordx2",
            S_LOAD_DWORDX4 => "s_load_dwordx4",
            S_LOAD_DWORDX8 => "s_load_dwordx8",
            S_LOAD_DWORDX16 => "s_load_dwordx16",
            S_BUFFER_LOAD_DWORD => "s_buffer_load_dword",
            S_BUFFER_LOAD_DWORDX2 => "s_buffer_load_dwordx2",
            S_BUFFER_LOAD_DWORDX4 => "s_buffer_load_dwordx4",
            S_BUFFER_LOAD_DWORDX8 => "s_buffer_load_dwordx8",
            S_BUFFER_LOAD_DWORDX16 => "s_buffer_load_dwordx16",
            _ => return None,
        })
    }

    /// Number of consecutive SGPR destinations the load writes (for `s[lo:hi]`
    /// disassembly). `None` for unmapped ops.
    pub fn dst_count(op: u8) -> Option<u8> {
        Some(match op {
            S_LOAD_DWORD | S_BUFFER_LOAD_DWORD => 1,
            S_LOAD_DWORDX2 | S_BUFFER_LOAD_DWORDX2 => 2,
            S_LOAD_DWORDX4 | S_BUFFER_LOAD_DWORDX4 => 4,
            S_LOAD_DWORDX8 | S_BUFFER_LOAD_DWORDX8 => 8,
            S_LOAD_DWORDX16 | S_BUFFER_LOAD_DWORDX16 => 16,
            _ => return None,
        })
    }

    /// Whether `op` is an `s_buffer_load` (SBASE names a 128-bit V# descriptor and
    /// the address is `V#.base + offset`) rather than an `s_load` (SBASE is a 64-bit
    /// pointer). The two decode identically but resolve their base address differently.
    pub fn is_buffer_load(op: u8) -> bool {
        matches!(
            op,
            S_BUFFER_LOAD_DWORD
                | S_BUFFER_LOAD_DWORDX2
                | S_BUFFER_LOAD_DWORDX4
                | S_BUFFER_LOAD_DWORDX8
                | S_BUFFER_LOAD_DWORDX16
        )
    }
}

/// VOPC (vector compare) opcodes — the compare writes a per-lane bool to VCC (the
/// standalone form) or to an arbitrary SGPR pair (the VOP3-encoded form). Only the
/// f32 compares the retail set reaches are named; the op field is `byte2 >> 1` with
/// byte3's low bit as the high op bit (f32 cmps prefix 0x7C, verified against llvm-mc
/// bonaire `-show-encoding`).
pub mod vopc {
    /// `v_cmp_lt_f32` — D = (S0 < S1). GFX7 op 1 (llvm-mc byte2 0x02 >> 1).
    pub const V_CMP_LT_F32: u8 = 0x01;
    /// `v_cmp_eq_f32` — D = (S0 == S1). GFX7 op 2 (llvm-mc byte2 0x04 >> 1).
    pub const V_CMP_EQ_F32: u8 = 0x02;
    /// `v_cmp_le_f32` — D = (S0 <= S1). GFX7 op 3 (llvm-mc byte2 0x06 >> 1).
    pub const V_CMP_LE_F32: u8 = 0x03;
    /// `v_cmp_gt_f32` — D = (S0 > S1). GFX7 op 4 (llvm-mc byte2 0x08 >> 1).
    pub const V_CMP_GT_F32: u8 = 0x04;
    /// `v_cmp_ge_f32` — D = (S0 >= S1). GFX7 op 6 (llvm-mc byte2 0x0C >> 1).
    pub const V_CMP_GE_F32: u8 = 0x06;

    // Signed 32-bit integer compares (VOPC op field 0x81..0x86, verified against
    // `llvm-mc -arch=amdgcn -mcpu=bonaire -show-encoding`: e.g. `v_cmp_eq_i32` encodes
    // to VOP3 byte2 0x04 / byte3 0xD1, whose op = bits[25:17] = 0x082). D = (S0 <cmp> S1)
    // as SIGNED integers; the 64-bit lane mask lands in the VOPC destination (VCC or an
    // SGPR pair). GFX7 Sea Islands ISA (§ VOPC opcodes).
    /// `v_cmp_lt_i32` — D = (S0 < S1) signed.
    pub const V_CMP_LT_I32: u8 = 0x81;
    /// `v_cmp_eq_i32` — D = (S0 == S1).
    pub const V_CMP_EQ_I32: u8 = 0x82;
    /// `v_cmp_le_i32` — D = (S0 <= S1) signed.
    pub const V_CMP_LE_I32: u8 = 0x83;
    /// `v_cmp_gt_i32` — D = (S0 > S1) signed.
    pub const V_CMP_GT_I32: u8 = 0x84;
    /// `v_cmp_ne_i32` (a.k.a. `v_cmp_lg_i32`) — D = (S0 != S1).
    pub const V_CMP_NE_I32: u8 = 0x85;
    /// `v_cmp_ge_i32` — D = (S0 >= S1) signed.
    pub const V_CMP_GE_I32: u8 = 0x86;

    // Unsigned 32-bit integer compares (VOPC op field 0xC1..0xC6, verified against
    // `llvm-mc -mcpu=bonaire`: e.g. `v_cmp_lt_u32` → VOP3 byte2 0x82 / byte3 0xD1,
    // op = bits[25:17] = 0x0C1). D = (S0 <cmp> S1) as UNSIGNED integers.
    /// `v_cmp_lt_u32` — D = (S0 < S1) unsigned.
    pub const V_CMP_LT_U32: u8 = 0xC1;
    /// `v_cmp_eq_u32` — D = (S0 == S1).
    pub const V_CMP_EQ_U32: u8 = 0xC2;
    /// `v_cmp_le_u32` — D = (S0 <= S1) unsigned.
    pub const V_CMP_LE_U32: u8 = 0xC3;
    /// `v_cmp_gt_u32` — D = (S0 > S1) unsigned.
    pub const V_CMP_GT_U32: u8 = 0xC4;
    /// `v_cmp_ne_u32` — D = (S0 != S1).
    pub const V_CMP_NE_U32: u8 = 0xC5;
    /// `v_cmp_ge_u32` — D = (S0 >= S1) unsigned.
    pub const V_CMP_GE_U32: u8 = 0xC6;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            V_CMP_LT_F32 => "v_cmp_lt_f32",
            V_CMP_EQ_F32 => "v_cmp_eq_f32",
            V_CMP_LE_F32 => "v_cmp_le_f32",
            V_CMP_GT_F32 => "v_cmp_gt_f32",
            V_CMP_GE_F32 => "v_cmp_ge_f32",
            V_CMP_LT_I32 => "v_cmp_lt_i32",
            V_CMP_EQ_I32 => "v_cmp_eq_i32",
            V_CMP_LE_I32 => "v_cmp_le_i32",
            V_CMP_GT_I32 => "v_cmp_gt_i32",
            V_CMP_NE_I32 => "v_cmp_ne_i32",
            V_CMP_GE_I32 => "v_cmp_ge_i32",
            V_CMP_LT_U32 => "v_cmp_lt_u32",
            V_CMP_EQ_U32 => "v_cmp_eq_u32",
            V_CMP_LE_U32 => "v_cmp_le_u32",
            V_CMP_GT_U32 => "v_cmp_gt_u32",
            V_CMP_NE_U32 => "v_cmp_ne_u32",
            V_CMP_GE_U32 => "v_cmp_ge_u32",
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
    /// `v_cvt_off_f32_i4` — `D.f = float(sext4(S0[3:0])) / 16.0`: the low 4 bits of the
    /// source are a signed [-8,7] integer mapped to the pixel-offset table [-0.5, 0.4375]
    /// in 1/16 steps (llvm-mc `v_cvt_off_f32_i4_e32 v10, 1` → byte1 0x1c >> 1 == 0x0E).
    pub const V_CVT_OFF_F32_I4: u8 = 0x0E;
    // GFX7/bonaire op field (llvm-mc encoding bits[16:9]).
    pub const V_FRACT_F32: u8 = 0x20;
    /// `v_ceil_f32` — `D.f = ceil(S0.f)` (llvm-mc `v_ceil_f32_e32` → byte1 0x45 >> 1 == 0x22).
    pub const V_CEIL_F32: u8 = 0x22;
    pub const V_FLOOR_F32: u8 = 0x24;
    pub const V_RCP_F32: u8 = 0x2A;
    pub const V_SQRT_F32: u8 = 0x33;
    /// GCN sine: `D.f = sin(2*PI*S0.f)` — the argument is in revolutions, so the 2*PI
    /// scaling is intrinsic (llvm-mc `v_sin_f32_e32`, byte1 0x6B >> 1 = 0x35).
    pub const V_SIN_F32: u8 = 0x35;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            V_NOP => "v_nop",
            V_MOV_B32 => "v_mov_b32",
            V_CVT_F32_I32 => "v_cvt_f32_i32",
            V_CVT_F32_U32 => "v_cvt_f32_u32",
            V_CVT_U32_F32 => "v_cvt_u32_f32",
            V_CVT_I32_F32 => "v_cvt_i32_f32",
            V_CVT_OFF_F32_I4 => "v_cvt_off_f32_i4",
            V_FRACT_F32 => "v_fract_f32",
            V_CEIL_F32 => "v_ceil_f32",
            V_FLOOR_F32 => "v_floor_f32",
            V_RCP_F32 => "v_rcp_f32",
            V_SQRT_F32 => "v_sqrt_f32",
            V_SIN_F32 => "v_sin_f32",
            _ => return None,
        })
    }
}

/// VOP2 (vector, two inputs) opcodes.
pub mod vop2 {
    /// `v_cndmask_b32` — D = VCC[lane] ? S1 : S0 (per-lane select on the predicate).
    /// GFX7 op 0 (llvm-mc `v_cndmask_b32_e32` → VOP2 op = byte3 0x00 >> 1 == 0).
    pub const V_CNDMASK_B32: u8 = 0x00;
    pub const V_ADD_F32: u8 = 0x03;
    pub const V_SUB_F32: u8 = 0x04;
    /// `v_subrev_f32` — reverse subtract: `D = S1 - S0` (llvm-mc `v_subrev_f32_e32` →
    /// byte3 0x0A >> 1 == 0x05). Same operands as `v_sub_f32`, subtrahend/minuend swapped.
    pub const V_SUBREV_F32: u8 = 0x05;
    pub const V_MUL_F32: u8 = 0x08;
    pub const V_MIN_F32: u8 = 0x0F;
    pub const V_MAX_F32: u8 = 0x10;
    pub const V_LSHRREV_B32: u8 = 0x16;
    pub const V_LSHLREV_B32: u8 = 0x1A;
    pub const V_AND_B32: u8 = 0x1B;
    pub const V_MAC_F32: u8 = 0x1F;
    pub const V_MADMK_F32: u8 = 0x20;
    pub const V_MADAK_F32: u8 = 0x21;
    /// `v_add_i32` — D = S0 + S1 (32-bit wrapping); carry-out to VCC. GFX7 op 0x25
    /// (llvm-mc `v_add_i32_e32` → VOP2 op = byte3 0x4A >> 1 == 0x25 = 37).
    pub const V_ADD_I32: u8 = 0x25;
    pub const V_CVT_PKRTZ_F16_F32: u8 = 0x2F;

    pub fn name(op: u8) -> Option<&'static str> {
        Some(match op {
            V_CNDMASK_B32 => "v_cndmask_b32",
            V_ADD_F32 => "v_add_f32",
            V_SUB_F32 => "v_sub_f32",
            V_SUBREV_F32 => "v_subrev_f32",
            V_MUL_F32 => "v_mul_f32",
            V_MIN_F32 => "v_min_f32",
            V_MAX_F32 => "v_max_f32",
            V_LSHRREV_B32 => "v_lshrrev_b32",
            V_LSHLREV_B32 => "v_lshlrev_b32",
            V_AND_B32 => "v_and_b32",
            V_MAC_F32 => "v_mac_f32",
            V_MADMK_F32 => "v_madmk_f32",
            V_MADAK_F32 => "v_madak_f32",
            V_ADD_I32 => "v_add_i32",
            V_CVT_PKRTZ_F16_F32 => "v_cvt_pkrtz_f16_f32",
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
/// the standard SI/CI ranges (verified against llvm-mc bonaire `-show-encoding`):
/// VOPC → 0x000, VOP2 → 0x100, native VOP3 → 0x140, VOP1 → 0x180. So a VOP1 op like
/// v_fract (VOP1 0x20) re-encoded as VOP3 (to carry abs/omod) lands at 0x180+0x20 =
/// 0x1A0. Only the ops the corpus exercises are named; others render numerically.
pub mod vop3 {
    /// v_cmp_lt_f32 re-encoded as VOP3 (VOPC 0x01, range 0x000) — writes the compare
    /// bool to an ARBITRARY SGPR pair (the `sdst` field), not just VCC. llvm-mc byte3
    /// 0xD0 (VOPC-in-VOP3), byte2 0x02 >> 1 == 0x01.
    pub const V_CMP_LT_F32: u16 = 0x001;
    /// v_cmp_eq_f32 re-encoded as VOP3 (VOPC 0x02, range 0x000). llvm-mc byte2 0x04 >>
    /// 1 == 0x02.
    pub const V_CMP_EQ_F32: u16 = 0x002;
    /// v_cmp_le_f32 re-encoded as VOP3 (VOPC 0x03, range 0x000). llvm-mc byte2 0x06 >>
    /// 1 == 0x03.
    pub const V_CMP_LE_F32: u16 = 0x003;
    /// v_cmp_gt_f32 re-encoded as VOP3 (VOPC 0x04, range 0x000). llvm-mc byte2 0x08 >>
    /// 1 == 0x04.
    pub const V_CMP_GT_F32: u16 = 0x004;
    /// v_cmp_ge_f32 re-encoded as VOP3 (VOPC 0x06, range 0x000). llvm-mc byte2 0x0C >>
    /// 1 == 0x06.
    pub const V_CMP_GE_F32: u16 = 0x006;
    // VOPC integer compares re-encoded as VOP3 (VOPC range 0x000, so the op number is
    // the raw VOPC op field). The compare bool lands in the ARBITRARY SGPR pair named
    // by the `sdst` field, not just VCC. Values verified against `llvm-mc -mcpu=bonaire
    // -show-encoding` (VOP3 op = bits[25:17]): `v_cmp_eq_i32` → 0x082, `v_cmp_lt_u32` →
    // 0x0C1, etc. — mirrors the `vopc` module's u8 constants.
    /// v_cmp_lt_i32 (VOP3B) — signed `S0 < S1` into an SGPR pair.
    pub const V_CMP_LT_I32: u16 = 0x081;
    /// v_cmp_eq_i32 (VOP3B) — `S0 == S1` into an SGPR pair.
    pub const V_CMP_EQ_I32: u16 = 0x082;
    /// v_cmp_le_i32 (VOP3B) — signed `S0 <= S1`.
    pub const V_CMP_LE_I32: u16 = 0x083;
    /// v_cmp_gt_i32 (VOP3B) — signed `S0 > S1`.
    pub const V_CMP_GT_I32: u16 = 0x084;
    /// v_cmp_ne_i32 (VOP3B) — `S0 != S1`.
    pub const V_CMP_NE_I32: u16 = 0x085;
    /// v_cmp_ge_i32 (VOP3B) — signed `S0 >= S1`.
    pub const V_CMP_GE_I32: u16 = 0x086;
    /// v_cmp_lt_u32 (VOP3B) — unsigned `S0 < S1`.
    pub const V_CMP_LT_U32: u16 = 0x0C1;
    /// v_cmp_eq_u32 (VOP3B) — `S0 == S1`.
    pub const V_CMP_EQ_U32: u16 = 0x0C2;
    /// v_cmp_le_u32 (VOP3B) — unsigned `S0 <= S1`.
    pub const V_CMP_LE_U32: u16 = 0x0C3;
    /// v_cmp_gt_u32 (VOP3B) — unsigned `S0 > S1`.
    pub const V_CMP_GT_U32: u16 = 0x0C4;
    /// v_cmp_ne_u32 (VOP3B) — `S0 != S1`.
    pub const V_CMP_NE_U32: u16 = 0x0C5;
    /// v_cmp_ge_u32 (VOP3B) — unsigned `S0 >= S1`.
    pub const V_CMP_GE_U32: u16 = 0x0C6;
    /// v_cndmask_b32 re-encoded as VOP3 (VOP2 0x00 + 0x100): `D = src2[lane] ? S1 : S0`
    /// — the predicate is an arbitrary SGPR pair in src2, not the implicit VCC of the
    /// VOP2 form. llvm-mc byte3 0xD2, byte2 0x00 >> 1 | 0x100 == 0x100.
    pub const V_CNDMASK_B32: u16 = 0x100;
    /// v_mul_f32 re-encoded as VOP3 (VOP2 0x08 + 0x100) — same `a * b` as the VOP2
    /// form, carrying abs/neg src modifiers + omod (llvm-mc byte2 0x10 >> 1 | 0x100).
    pub const V_MUL_F32: u16 = 0x108;
    /// v_cvt_pkrtz_f16_f32 re-encoded as VOP3 (VOP2 0x2F + 0x100): pack src0/src1 as
    /// two f16 into a u32 (`D[15:0]=f16(S0)`, `D[31:16]=f16(S1)`). llvm-mc byte2 0x5E.
    pub const V_CVT_PKRTZ_F16_F32: u16 = 0x12F;
    /// v_mac_f32 re-encoded as VOP3 (VOP2 0x1F + 0x100): `D = S0*S1 + D` — the dst is
    /// an implicit accumulator (read-modify-write), UNFUSED (mul rounds, then add
    /// rounds). llvm-mc byte2 0x3E >> 1 | 0x100.
    pub const V_MAC_F32: u16 = 0x11F;
    pub const V_MAD_F32: u16 = 0x141;
    /// v_mad_u32_u24: `D = (S0[23:0] * S1[23:0]) + S2` (unsigned 24-bit multiply-add,
    /// 32-bit wrapping). Native VOP3-only integer op (byte2 0x86 >> 1 | 0x100).
    pub const V_MAD_U32_U24: u16 = 0x143;
    pub const V_FMA_F32: u16 = 0x14B;
    pub const V_MED3_F32: u16 = 0x157;
    /// v_fract_f32 re-encoded as VOP3 (VOP1 0x20 + 0x180) — same `x - floor(x)` as the
    /// VOP1 form, but carries the abs src modifier and the omod output scale.
    pub const V_FRACT_F32: u16 = 0x1A0;

    pub fn name(op: u16) -> Option<&'static str> {
        Some(match op {
            V_CMP_LT_F32 => "v_cmp_lt_f32",
            V_CMP_EQ_F32 => "v_cmp_eq_f32",
            V_CMP_LE_F32 => "v_cmp_le_f32",
            V_CMP_GT_F32 => "v_cmp_gt_f32",
            V_CMP_GE_F32 => "v_cmp_ge_f32",
            V_CMP_LT_I32 => "v_cmp_lt_i32",
            V_CMP_EQ_I32 => "v_cmp_eq_i32",
            V_CMP_LE_I32 => "v_cmp_le_i32",
            V_CMP_GT_I32 => "v_cmp_gt_i32",
            V_CMP_NE_I32 => "v_cmp_ne_i32",
            V_CMP_GE_I32 => "v_cmp_ge_i32",
            V_CMP_LT_U32 => "v_cmp_lt_u32",
            V_CMP_EQ_U32 => "v_cmp_eq_u32",
            V_CMP_LE_U32 => "v_cmp_le_u32",
            V_CMP_GT_U32 => "v_cmp_gt_u32",
            V_CMP_NE_U32 => "v_cmp_ne_u32",
            V_CMP_GE_U32 => "v_cmp_ge_u32",
            V_CNDMASK_B32 => "v_cndmask_b32",
            V_MUL_F32 => "v_mul_f32",
            V_CVT_PKRTZ_F16_F32 => "v_cvt_pkrtz_f16_f32",
            V_MAC_F32 => "v_mac_f32",
            V_MAD_F32 => "v_mad_f32",
            V_MAD_U32_U24 => "v_mad_u32_u24",
            V_FMA_F32 => "v_fma_f32",
            V_MED3_F32 => "v_med3_f32",
            V_FRACT_F32 => "v_fract_f32",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins every named GCN op number to its AMD Sea Islands (GFX7) hardware opcode.
    ///
    /// The right-hand literals are the microcode opcodes in the AMD Sea Islands ISA
    /// (`oracles/amd/ci-isa.pdf`, per-format "Microcode <FMT> Opcode N"
    /// instruction tables), independently reproduced by assembling each mnemonic with
    /// `llvm-mc --assemble --arch=amdgcn --mcpu=gfx700 --show-encoding` and reading the
    /// op field out of the encoding bytes (bit ranges per class named in the module
    /// doc). The trailing hex in each `//` comment is the `llvm-mc` encoding those bytes
    /// came from. This test fails if any of our constants drift from that AMD value.
    #[test]
    fn gcn_opcodes_match_amd_oracle() {
        // SOP1 — op = bits[15:8] = encoding byte1. e.g. s_setpc_b64 → [0x02,0x20,0x80,0xbe].
        assert_eq!(sop1::S_MOV_B32, 0x03); //  [0x01,0x03,0x80,0xbe]
        assert_eq!(sop1::S_MOV_B64, 0x04); //  [0x02,0x04,0x80,0xbe]
        assert_eq!(sop1::S_WQM_B64, 0x0A); //  [0x02,0x0a,0x80,0xbe]
        assert_eq!(sop1::S_SETPC_B64, 0x20); // [0x02,0x20,0x80,0xbe]
        assert_eq!(sop1::S_SWAPPC_B64, 0x21); // [0x02,0x21,0x80,0xbe]

        // SOP2 — op = bits[29:23]. e.g. s_lshl_b32 → [0x01,0x02,0x00,0x8f], 0x8f000201>>23 & 0x7F.
        assert_eq!(sop2::S_ADD_U32, 0x00); // [0x01,0x02,0x00,0x80]
        assert_eq!(sop2::S_SUB_U32, 0x01); // [0x01,0x02,0x80,0x80]
        assert_eq!(sop2::S_ADD_I32, 0x02); // [0x01,0x02,0x00,0x81]
        assert_eq!(sop2::S_AND_B32, 0x0E); // [0x01,0x02,0x00,0x87]
        assert_eq!(sop2::S_OR_B32, 0x10); //  [0x01,0x02,0x00,0x88]
        assert_eq!(sop2::S_LSHL_B32, 0x1E); // [0x01,0x02,0x00,0x8f]

        // SOPK — op = bits[27:23]. e.g. s_cmpk_eq_i32 → [0x34,0x12,0x80,0xb1].
        assert_eq!(sopk::S_MOVK_I32, 0x00); //   [0x34,0x12,0x00,0xb0]
        assert_eq!(sopk::S_CMPK_EQ_I32, 0x03); // [0x34,0x12,0x80,0xb1]

        // SOPC — op = bits[22:16] = byte2 & 0x7F.
        assert_eq!(sopc::S_CMP_EQ_I32, 0x00); // [0x00,0x01,0x00,0xbf]
        assert_eq!(sopc::S_CMP_LG_I32, 0x01); // [0x00,0x01,0x01,0xbf]

        // SOPP — op = bits[22:16] = byte2 & 0x7F. e.g. s_endpgm → [0x00,0x00,0x81,0xbf].
        assert_eq!(sopp::S_NOP, 0x00); //           byte2 0x80
        assert_eq!(sopp::S_ENDPGM, 0x01); //        byte2 0x81
        assert_eq!(sopp::S_BRANCH, 0x02); //        byte2 0x82
        assert_eq!(sopp::S_CBRANCH_SCC0, 0x04); //  byte2 0x84
        assert_eq!(sopp::S_CBRANCH_SCC1, 0x05); //  byte2 0x85
        assert_eq!(sopp::S_CBRANCH_VCCZ, 0x06); //  byte2 0x86
        assert_eq!(sopp::S_CBRANCH_VCCNZ, 0x07); // byte2 0x87
        assert_eq!(sopp::S_CBRANCH_EXECZ, 0x08); // byte2 0x88
        assert_eq!(sopp::S_CBRANCH_EXECNZ, 0x09); // byte2 0x89
        assert_eq!(sopp::S_BARRIER, 0x0A); //       byte2 0x8a
        assert_eq!(sopp::S_WAITCNT, 0x0C); //       byte2 0x8c

        // SMRD — op = bits[26:22]. e.g. s_buffer_load_dwordx16 → [0x00,0x05,0x00,0xc3].
        assert_eq!(smrd::S_LOAD_DWORD, 0x00); //            [0x00,0x03,0x00,0xc0]
        assert_eq!(smrd::S_LOAD_DWORDX2, 0x01); //          [0x00,0x03,0x40,0xc0]
        assert_eq!(smrd::S_LOAD_DWORDX4, 0x02); //          [0x00,0x03,0x80,0xc0]
        assert_eq!(smrd::S_LOAD_DWORDX8, 0x03); //          [0x00,0x03,0xc0,0xc0]
        assert_eq!(smrd::S_LOAD_DWORDX16, 0x04); //         [0x00,0x03,0x00,0xc1]
        assert_eq!(smrd::S_BUFFER_LOAD_DWORD, 0x08); //     [0x00,0x05,0x00,0xc2]
        assert_eq!(smrd::S_BUFFER_LOAD_DWORDX2, 0x09); //   [0x00,0x05,0x40,0xc2]
        assert_eq!(smrd::S_BUFFER_LOAD_DWORDX4, 0x0A); //   [0x00,0x05,0x80,0xc2]
        assert_eq!(smrd::S_BUFFER_LOAD_DWORDX8, 0x0B); //   [0x00,0x05,0xc0,0xc2]
        assert_eq!(smrd::S_BUFFER_LOAD_DWORDX16, 0x0C); //  [0x00,0x05,0x00,0xc3]

        // VOPC — op = bits[24:17] = ((byte3 & 1) << 7) | (byte2 >> 1). f32 prefix byte3
        // 0x7c, i32/u32 prefix byte3 0x7d. e.g. v_cmp_eq_i32 → [0x00,0x03,0x04,0x7d] = 0x82.
        assert_eq!(vopc::V_CMP_LT_F32, 0x01); // [0x00,0x03,0x02,0x7c]
        assert_eq!(vopc::V_CMP_EQ_F32, 0x02); // [0x00,0x03,0x04,0x7c]
        assert_eq!(vopc::V_CMP_LE_F32, 0x03); // [0x00,0x03,0x06,0x7c]
        assert_eq!(vopc::V_CMP_GT_F32, 0x04); // [0x00,0x03,0x08,0x7c]
        assert_eq!(vopc::V_CMP_GE_F32, 0x06); // [0x00,0x03,0x0c,0x7c]
        assert_eq!(vopc::V_CMP_LT_I32, 0x81); // [0x00,0x03,0x02,0x7d]
        assert_eq!(vopc::V_CMP_EQ_I32, 0x82); // [0x00,0x03,0x04,0x7d]
        assert_eq!(vopc::V_CMP_LE_I32, 0x83); // [0x00,0x03,0x06,0x7d]
        assert_eq!(vopc::V_CMP_GT_I32, 0x84); // [0x00,0x03,0x08,0x7d]
        assert_eq!(vopc::V_CMP_NE_I32, 0x85); // [0x00,0x03,0x0a,0x7d]
        assert_eq!(vopc::V_CMP_GE_I32, 0x86); // [0x00,0x03,0x0c,0x7d]
        assert_eq!(vopc::V_CMP_LT_U32, 0xC1); // [0x00,0x03,0x82,0x7d]
        assert_eq!(vopc::V_CMP_EQ_U32, 0xC2); // [0x00,0x03,0x84,0x7d]
        assert_eq!(vopc::V_CMP_LE_U32, 0xC3); // [0x00,0x03,0x86,0x7d]
        assert_eq!(vopc::V_CMP_GT_U32, 0xC4); // [0x00,0x03,0x88,0x7d]
        assert_eq!(vopc::V_CMP_NE_U32, 0xC5); // [0x00,0x03,0x8a,0x7d]
        assert_eq!(vopc::V_CMP_GE_U32, 0xC6); // [0x00,0x03,0x8c,0x7d]

        // VOP1 — op = bits[16:9] = byte1 >> 1. e.g. v_sqrt_f32 → [0x01,0x67,0x00,0x7e].
        assert_eq!(vop1::V_NOP, 0x00); //           [0x00,0x00,0x00,0x7e]
        assert_eq!(vop1::V_MOV_B32, 0x01); //       [0x01,0x03,0x00,0x7e]
        assert_eq!(vop1::V_CVT_F32_I32, 0x05); //   [0x01,0x0b,0x00,0x7e]
        assert_eq!(vop1::V_CVT_F32_U32, 0x06); //   [0x01,0x0d,0x00,0x7e]
        assert_eq!(vop1::V_CVT_U32_F32, 0x07); //   [0x01,0x0f,0x00,0x7e]
        assert_eq!(vop1::V_CVT_I32_F32, 0x08); //   [0x01,0x11,0x00,0x7e]
        assert_eq!(vop1::V_CVT_OFF_F32_I4, 0x0E); // [0x01,0x1d,0x00,0x7e]
        assert_eq!(vop1::V_FRACT_F32, 0x20); //     [0x01,0x41,0x00,0x7e]
        assert_eq!(vop1::V_CEIL_F32, 0x22); //      [0x01,0x45,0x00,0x7e]
        assert_eq!(vop1::V_FLOOR_F32, 0x24); //     [0x01,0x49,0x00,0x7e]
        assert_eq!(vop1::V_RCP_F32, 0x2A); //       [0x01,0x55,0x00,0x7e]
        assert_eq!(vop1::V_SQRT_F32, 0x33); //      [0x01,0x67,0x00,0x7e]
        assert_eq!(vop1::V_SIN_F32, 0x35); //       [0x01,0x6b,0x00,0x7e]

        // VOP2 — op = bits[30:25] = byte3 >> 1. e.g. v_add_i32 → [0x01,0x05,0x00,0x4a].
        assert_eq!(vop2::V_CNDMASK_B32, 0x00); //       byte3 0x00
        assert_eq!(vop2::V_ADD_F32, 0x03); //           byte3 0x06
        assert_eq!(vop2::V_SUB_F32, 0x04); //           byte3 0x08
        assert_eq!(vop2::V_SUBREV_F32, 0x05); //        byte3 0x0a
        assert_eq!(vop2::V_MUL_F32, 0x08); //           byte3 0x10
        assert_eq!(vop2::V_MIN_F32, 0x0F); //           byte3 0x1e
        assert_eq!(vop2::V_MAX_F32, 0x10); //           byte3 0x20
        assert_eq!(vop2::V_LSHRREV_B32, 0x16); //       byte3 0x2c
        assert_eq!(vop2::V_LSHLREV_B32, 0x1A); //       byte3 0x34
        assert_eq!(vop2::V_AND_B32, 0x1B); //           byte3 0x36
        assert_eq!(vop2::V_MAC_F32, 0x1F); //           byte3 0x3e
        assert_eq!(vop2::V_MADMK_F32, 0x20); //         byte3 0x40
        assert_eq!(vop2::V_MADAK_F32, 0x21); //         byte3 0x42
        assert_eq!(vop2::V_ADD_I32, 0x25); //           byte3 0x4a
        assert_eq!(vop2::V_CVT_PKRTZ_F16_F32, 0x2F); // byte3 0x5e

        // VOP3 — op = bits[25:17] = ((byte3 & 3) << 7) | (byte2 >> 1). VOPC→0x000,
        // VOP2→0x100, native VOP3→0x140, VOP1→0x180. e.g. v_mad_f32 → byte2 0x82,
        // byte3 0xd2 = (2<<7)|(0x82>>1) = 0x141.
        assert_eq!(vop3::V_CMP_LT_F32, 0x001); // byte2 0x02 byte3 0xd0
        assert_eq!(vop3::V_CMP_EQ_F32, 0x002); // byte2 0x04 byte3 0xd0
        assert_eq!(vop3::V_CMP_LE_F32, 0x003); // byte2 0x06 byte3 0xd0
        assert_eq!(vop3::V_CMP_GT_F32, 0x004); // byte2 0x08 byte3 0xd0
        assert_eq!(vop3::V_CMP_GE_F32, 0x006); // byte2 0x0c byte3 0xd0
        assert_eq!(vop3::V_CMP_LT_I32, 0x081); // byte2 0x02 byte3 0xd1
        assert_eq!(vop3::V_CMP_EQ_I32, 0x082); // byte2 0x04 byte3 0xd1
        assert_eq!(vop3::V_CMP_LE_I32, 0x083); // byte2 0x06 byte3 0xd1
        assert_eq!(vop3::V_CMP_GT_I32, 0x084); // byte2 0x08 byte3 0xd1
        assert_eq!(vop3::V_CMP_NE_I32, 0x085); // byte2 0x0a byte3 0xd1
        assert_eq!(vop3::V_CMP_GE_I32, 0x086); // byte2 0x0c byte3 0xd1
        assert_eq!(vop3::V_CMP_LT_U32, 0x0C1); // byte2 0x82 byte3 0xd1
        assert_eq!(vop3::V_CMP_EQ_U32, 0x0C2); // byte2 0x84 byte3 0xd1
        assert_eq!(vop3::V_CMP_LE_U32, 0x0C3); // byte2 0x86 byte3 0xd1
        assert_eq!(vop3::V_CMP_GT_U32, 0x0C4); // byte2 0x88 byte3 0xd1
        assert_eq!(vop3::V_CMP_NE_U32, 0x0C5); // byte2 0x8a byte3 0xd1
        assert_eq!(vop3::V_CMP_GE_U32, 0x0C6); // byte2 0x8c byte3 0xd1
        assert_eq!(vop3::V_CNDMASK_B32, 0x100); //       byte2 0x00 byte3 0xd2
        assert_eq!(vop3::V_MUL_F32, 0x108); //           byte2 0x10 byte3 0xd2
        assert_eq!(vop3::V_CVT_PKRTZ_F16_F32, 0x12F); // byte2 0x5e byte3 0xd2
        assert_eq!(vop3::V_MAC_F32, 0x11F); //           byte2 0x3e byte3 0xd2
        assert_eq!(vop3::V_MAD_F32, 0x141); //           byte2 0x82 byte3 0xd2
        assert_eq!(vop3::V_MAD_U32_U24, 0x143); //       byte2 0x86 byte3 0xd2
        assert_eq!(vop3::V_FMA_F32, 0x14B); //           byte2 0x96 byte3 0xd2
        assert_eq!(vop3::V_MED3_F32, 0x157); //          byte2 0xae byte3 0xd2
        assert_eq!(vop3::V_FRACT_F32, 0x1A0); //         byte2 0x40 byte3 0xd3

        // MIMG — op = bits[24:18]. e.g. image_sample → [0x00,0x0f,0x80,0xf0,..].
        assert_eq!(mimg::IMAGE_LOAD, 0x00); //   byte2 0x00 byte3 0xf0
        assert_eq!(mimg::IMAGE_STORE, 0x08); //  byte2 0x20 byte3 0xf0
        assert_eq!(mimg::IMAGE_SAMPLE, 0x20); // byte2 0x80 byte3 0xf0

        // VINTRP — op = bits[17:16] = byte2 & 0x3.
        assert_eq!(vintrp::V_INTERP_P1_F32, 0x00); //  [0x01,0x00,0x00,0xc8]
        assert_eq!(vintrp::V_INTERP_P2_F32, 0x01); //  [0x01,0x00,0x01,0xc8]
        assert_eq!(vintrp::V_INTERP_MOV_F32, 0x02); // [0x02,0x00,0x02,0xc8]

        // MUBUF — op = bits[24:18]. e.g. buffer_store_dword → [0x00,0x20,0x70,0xe0,..].
        assert_eq!(mubuf::BUFFER_LOAD_FORMAT_X, 0x00); //    byte2 0x00 byte3 0xe0
        assert_eq!(mubuf::BUFFER_LOAD_FORMAT_XY, 0x01); //   byte2 0x04 byte3 0xe0
        assert_eq!(mubuf::BUFFER_LOAD_FORMAT_XYZ, 0x02); //  byte2 0x08 byte3 0xe0
        assert_eq!(mubuf::BUFFER_LOAD_FORMAT_XYZW, 0x03); // byte2 0x0c byte3 0xe0
        assert_eq!(mubuf::BUFFER_STORE_FORMAT_X, 0x04); //   byte2 0x10 byte3 0xe0
        assert_eq!(mubuf::BUFFER_LOAD_DWORD, 0x0C); //       byte2 0x30 byte3 0xe0
        assert_eq!(mubuf::BUFFER_STORE_DWORD, 0x1C); //      byte2 0x70 byte3 0xe0
    }

    /// The `name()` lookups are glue over the constants pinned above — spot-check that a
    /// couple round-trip and that an unmapped op yields `None` (numeric fallback).
    #[test]
    fn name_lookups_resolve_and_fall_back() {
        assert_eq!(vop1::name(vop1::V_SQRT_F32), Some("v_sqrt_f32"));
        assert_eq!(vop3::name(vop3::V_MAD_F32), Some("v_mad_f32"));
        assert_eq!(sopp::name(sopp::S_ENDPGM), Some("s_endpgm"));
        assert_eq!(vop1::name(0xFF), None);
        assert_eq!(vop3::name(0x1FF), None);
    }
}
