//! Shared per-opcode ALU semantics — the tagless-final "write it once" layer
//! (task-131).
//!
//! GCN opcode semantics used to be written TWICE by hand: once in [`crate::interp`]
//! (the wave64 CPU oracle) and once in [`crate::recompile`] (the per-invocation
//! SPIR-V emitter), kept in sync only by golden tests and the differential
//! CPU-vs-SPIR-V oracle. This module extracts the *uniformly-f32 ALU* — the exact
//! subset where the two hand copies were byte-for-byte the same computation — into a
//! single generic body parameterised over an abstract value algebra.
//!
//! ## The split that STAYS
//!
//! The two backends genuinely differ at the *execution-model* level and that split
//! is deliberate and out of scope here:
//!
//! - The interp runs a **wave64** loop, EXEC-gating each lane, reading/writing raw
//!   `u32` VGPR bits and reinterpreting them as f32 per op.
//! - The recompiler emits **per-invocation** SPIR-V (one lane), with f32-typed value
//!   ids, vertex-index provenance tracking, and predicate bools.
//!
//! Only the *value algebra* inside a live lane — `a + b`, `a * b`, `mul_add`,
//! `median3`, the abs/neg/omod modifiers — is identical, and only that is shared.
//! Each backend implements [`AluBuilder`] over its own `Val` (interp: raw `u32`
//! bits reinterpreted per op; recompiler: an f32-typed `spirv::Word`) and the two
//! keep driving their own lane loop / straight-line emission around the shared body.
//!
//! ## What is NOT here (kept hand-written in BOTH backends, in lockstep)
//!
//! - **VOP1** (`v_mov_b32`, `v_cvt_*`, transcendentals): `v_mov` carries
//!   recompiler-only vertex-index tracking, and the conversions mix the int/float
//!   type domains — the uniform-f32 `Val` algebra does not model them.
//! - **Integer / bitwise VOP2/VOP3** (`v_and`, `v_lshlrev`, `v_lshrrev`,
//!   `v_add_i32`, `v_mad_u32_u24`, `v_cndmask`): raw-bits domain, and several touch
//!   the predicate (VCC / SGPR-pair) machinery that is execution-model-specific.
//! - **VOPC f32 compares** and the **VOP3-form compares**: produce a per-lane bool
//!   into a predicate destination whose *representation* (a 64-bit wave mask bit vs.
//!   a single SPIR-V `bool`) is the execution-model split itself.
//! - **`v_cvt_pkrtz_f16_f32`**: packs two f16 into a u32 — a bits-producing op whose
//!   host (`half::f16::from_f32`) and SPIR-V (`PackHalf2x16`) lowerings are unrelated
//!   at the value level even though they agree numerically.
//! - **SMEM / MUBUF / VINTRP / MIMG / EXP**: single-implementation or structurally
//!   different, out of scope.
//!
//! ## Acceptance
//!
//! Adding a new uniformly-f32 VOP2/VOP3 op now needs *one* arm in this file (plus its
//! opcode constant and the `is_uop_*` gate), not two hand-mirrored arms. The golden
//! `spirv-dis` disassembly and the differential value oracle are the correctness
//! fence: the shared body emits the identical instruction sequence each hand arm did.

use crate::opcodes;

/// The value algebra both backends already had internally, lifted to a trait.
///
/// `Val` is the backend's notion of an f32 value: raw `u32` bits (reinterpreted per
/// op) in the interp, an f32-typed `spirv::Word` in the recompiler. Every method is
/// the EXACT operation the corresponding hand arm performed — `f_add` is the interp's
/// `a + b` / the recompiler's `OpFAdd`, `f_fma` is `f32::mul_add` / GLSL `Fma`, etc.
/// The generic per-opcode bodies below call only these, so the two backends can never
/// drift on the ALU semantics.
pub(crate) trait AluBuilder {
    /// The backend's f32 value representation.
    type Val: Copy;

    /// Materialise the f32 whose IEEE-754 bit pattern is `bits`.
    fn const_f32_bits(&mut self, bits: u32) -> Self::Val;

    /// `a + b` — one rounding.
    fn f_add(&mut self, a: Self::Val, b: Self::Val) -> Self::Val;
    /// `a - b` — one rounding.
    fn f_sub(&mut self, a: Self::Val, b: Self::Val) -> Self::Val;
    /// `a * b` — one rounding.
    fn f_mul(&mut self, a: Self::Val, b: Self::Val) -> Self::Val;
    /// Non-NaN-preserving minimum (host `f32::min` / GLSL `FMin`).
    fn f_min(&mut self, a: Self::Val, b: Self::Val) -> Self::Val;
    /// Non-NaN-preserving maximum (host `f32::max` / GLSL `FMax`).
    fn f_max(&mut self, a: Self::Val, b: Self::Val) -> Self::Val;
    /// FUSED multiply-add `a*b + c` — a single rounding (host `mul_add` / GLSL `Fma`).
    fn f_fma(&mut self, a: Self::Val, b: Self::Val, c: Self::Val) -> Self::Val;
    /// Absolute value.
    fn f_abs(&mut self, a: Self::Val) -> Self::Val;
    /// Negation.
    fn f_neg(&mut self, a: Self::Val) -> Self::Val;
    /// `fract(x)` clamped to `[0, 1)` — `x - floor(x)` capped at the largest f32 below
    /// `1.0`. The interp does this arithmetically; the recompiler lowers to GLSL
    /// `Fract` + a `>= 1.0` clamping select. Both agree bit-for-bit (see
    /// [`crate::interp`]'s `fract_f32` / [`crate::recompile`]'s `emit_fract`).
    fn f_fract(&mut self, a: Self::Val) -> Self::Val;
}

/// Whether a VOP2 op is handled by the shared uniformly-f32 body ([`eval_vop2`]).
///
/// These are the ops whose two hand copies computed the identical f32 expression.
/// Everything else in VOP2 (integer/bitwise, cndmask, pkrtz) stays hand-written in
/// each backend — see the module docs.
pub(crate) fn is_uop_vop2(op: u8) -> bool {
    use opcodes::vop2::*;
    matches!(
        op,
        V_ADD_F32
            | V_SUB_F32
            | V_SUBREV_F32
            | V_MUL_F32
            | V_MIN_F32
            | V_MAX_F32
            | V_MAC_F32
            | V_MADMK_F32
            | V_MADAK_F32
    )
}

/// Whether a VOP3 op is handled by the shared uniformly-f32 body ([`eval_vop3`]).
///
/// The compares, `v_cndmask`, `v_mad_u32_u24` and `v_cvt_pkrtz` are excluded — they
/// are not the uniform-f32 algebra (see the module docs).
pub(crate) fn is_uop_vop3(op: u16) -> bool {
    use opcodes::vop3::*;
    matches!(
        op,
        V_MUL_F32 | V_MAC_F32 | V_MAD_F32 | V_FMA_F32 | V_MED3_F32 | V_FRACT_F32
    )
}

/// Median-of-three: `min(max(a,b), max(min(a,b), c))` — algebraically the median.
///
/// The argument-evaluation order is load-bearing for the golden `spirv-dis`: it emits
/// `FMAX(a,b)`, then `FMIN(a,b)`, then `FMAX(min,c)`, then the outer `FMIN`. Emitting
/// each into a `let` first pins that sequence; do NOT collapse into nested calls.
fn median3<B: AluBuilder>(b: &mut B, a: B::Val, bb: B::Val, c: B::Val) -> B::Val {
    let max_ab = b.f_max(a, bb);
    let min_ab = b.f_min(a, bb);
    let max_minab_c = b.f_max(min_ab, c);
    b.f_min(max_ab, max_minab_c)
}

/// Apply the VOP3 per-source `abs`/`neg` modifiers to input index `idx` (abs first,
/// then neg), matching both backends' `apply_mods`.
pub(crate) fn apply_mods<B: AluBuilder>(
    b: &mut B,
    mut v: B::Val,
    abs: u8,
    neg: u8,
    idx: u8,
) -> B::Val {
    if abs & (1 << idx) != 0 {
        v = b.f_abs(v);
    }
    if neg & (1 << idx) != 0 {
        v = b.f_neg(v);
    }
    v
}

/// Apply the VOP3 output modifier: 1 = ×2, 2 = ×4, 3 = ÷2 (0 = none). Multiplication
/// by an exact power of two is bit-exact, so both backends agree.
pub(crate) fn apply_omod<B: AluBuilder>(b: &mut B, v: B::Val, omod: u8) -> B::Val {
    let factor_bits = match omod {
        1 => 2.0f32.to_bits(),
        2 => 4.0f32.to_bits(),
        3 => 0.5f32.to_bits(),
        _ => return v,
    };
    let f = b.const_f32_bits(factor_bits);
    b.f_mul(v, f)
}

/// Apply the VOP3 `clamp` output modifier: saturate to `[0.0, 1.0]`.
///
/// Comes AFTER [`apply_omod`] in the hardware's output chain (result → omod → clamp),
/// so `mul:2 clamp` scales first and saturates the scaled value — the reverse order
/// would silently change the result of every instruction carrying both.
///
/// Lowered as `min(max(v, 0.0), 1.0)` out of the SAME [`AluBuilder::f_max`] /
/// [`AluBuilder::f_min`] the `v_max_f32`/`v_min_f32` ops already use, rather than a
/// GLSL `FClamp` in the recompiler: `FClamp` is *defined* as that composition, and
/// reusing the existing pair keeps the two backends on primitives the differential
/// oracle already pins — including their NaN behaviour. That behaviour is the
/// hardware's: GCN's clamp is min/max-based, `max(NaN, 0.0)` returns the non-NaN
/// operand, so a NaN result saturates to `0.0` (host `f32::max` agrees).
pub(crate) fn apply_clamp<B: AluBuilder>(b: &mut B, v: B::Val, clamp: bool) -> B::Val {
    if !clamp {
        return v;
    }
    let zero = b.const_f32_bits(0.0f32.to_bits());
    let one = b.const_f32_bits(1.0f32.to_bits());
    let lo = b.f_max(v, zero);
    b.f_min(lo, one)
}

/// Shared body for the uniformly-f32 VOP2 ops. `a`/`bb` are the two sources (already
/// read as f32 by the backend), `dst_old` is the current destination value (only used
/// by `v_mac`, the read-modify-write accumulator), and `k` is the inline literal (only
/// used by `v_madmk`/`v_madak`). Precondition: [`is_uop_vop2`]`(op)` is true.
pub(crate) fn eval_vop2<B: AluBuilder>(
    b: &mut B,
    op: u8,
    a: B::Val,
    bb: B::Val,
    dst_old: B::Val,
    k: Option<u32>,
) -> B::Val {
    use opcodes::vop2::*;
    match op {
        V_ADD_F32 => b.f_add(a, bb),
        V_SUB_F32 => b.f_sub(a, bb),
        // v_subrev_f32: D = S1 - S0 (reverse subtract) — sources swapped vs v_sub_f32.
        V_SUBREV_F32 => b.f_sub(bb, a),
        V_MUL_F32 => b.f_mul(a, bb),
        V_MIN_F32 => b.f_min(a, bb),
        V_MAX_F32 => b.f_max(a, bb),
        // v_mac_f32: D = S0*S1 + D — dst is an implicit accumulator. UNFUSED (mul
        // rounds, then add rounds), so `f_mul` then `f_add`, never `f_fma`.
        V_MAC_F32 => {
            let m = b.f_mul(a, bb);
            b.f_add(m, dst_old)
        }
        // v_madmk_f32: D = S0*K + S1. UNFUSED.
        V_MADMK_F32 => {
            let kf = b.const_f32_bits(k.unwrap_or(0));
            let m = b.f_mul(a, kf);
            b.f_add(m, bb)
        }
        // v_madak_f32 (the only op left in the gate): D = S0*S1 + K. UNFUSED.
        _ => {
            let kf = b.const_f32_bits(k.unwrap_or(0));
            let m = b.f_mul(a, bb);
            b.f_add(m, kf)
        }
    }
}

/// Shared body for the uniformly-f32 VOP3 ops. `a`/`bb`/`c` are the three sources
/// with abs/neg ALREADY applied by the caller (via [`apply_mods`]); `dst_old` is the
/// current destination (only `v_mac` reads it). The caller applies [`apply_omod`] then
/// [`apply_clamp`] to the returned value. Precondition: [`is_uop_vop3`]`(op)` is true.
pub(crate) fn eval_vop3<B: AluBuilder>(
    b: &mut B,
    op: u16,
    a: B::Val,
    bb: B::Val,
    c: B::Val,
    dst_old: B::Val,
) -> B::Val {
    use opcodes::vop3::*;
    match op {
        // v_mul_f32 (VOP3 form): a*b — only src0/src1 used.
        V_MUL_F32 => b.f_mul(a, bb),
        // v_mac_f32: D = S0*S1 + D. UNFUSED (mul then add), reads the old dst.
        V_MAC_F32 => {
            let m = b.f_mul(a, bb);
            b.f_add(m, dst_old)
        }
        // v_mad_f32 is UNFUSED on GCN: a*b rounds, then +c rounds again.
        V_MAD_F32 => {
            let m = b.f_mul(a, bb);
            b.f_add(m, c)
        }
        // v_fma_f32 is FUSED: a single rounding of a*b+c.
        V_FMA_F32 => b.f_fma(a, bb, c),
        // v_fract_f32 (VOP3 form) = x - floor(x), clamped to [0,1). Only src0 used.
        V_FRACT_F32 => b.f_fract(a),
        // v_med3_f32 (the only op left in the gate).
        _ => median3(b, a, bb, c),
    }
}
