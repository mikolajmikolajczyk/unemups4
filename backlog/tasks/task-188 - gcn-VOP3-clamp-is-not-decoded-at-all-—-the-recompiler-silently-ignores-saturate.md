---
id: TASK-188
title: >-
  gcn: VOP3 clamp is not decoded at all — the recompiler silently ignores
  saturate
status: Done
assignee: []
created_date: '2026-07-20 18:18'
updated_date: '2026-07-23 18:39'
labels:
  - gcn
  - recompiler
  - correctness
dependencies: []
priority: medium
ordinal: 192000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The VOP3 clamp bit (w0 bit 11) is never read: decode_vop3 does not extract it, Inst::Vop3 has no field for it, and neither the interpreter nor the recompiler consumes it. So a shader that asks for a saturated result — clamp to [0,1], what HLSL/GLSL saturate() compiles to — gets an unclamped one from us. Unlike task-182 (which was a display-only defect: neg/abs were decoded and applied, merely not printed), this is a CORRECTNESS gap in the lowering itself.\n\nWhy it has probably not bitten yet: when the clamped value is exported straight to a UNORM render target, the format clamps on store anyway, so the output is identical and the bug is invisible. It becomes real wherever the value is CONSUMED FURTHER inside the shader — as a lerp factor, an exponent, a multiplier, or a texture coordinate — and for any float-format target, where nothing clamps for us.\n\nA crude scan of the dumped Celeste shader corpus found the clamp bit set on one VOP3 (ps-27883c9d7c88cd30.sb). Treat that as approximate: the scan did not walk true instruction lengths, so its alignment can drift. Confirm with the real decoder as step one — if Celeste genuinely uses it, that also gives a concrete case to verify the fix against.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 VOP3 clamp is decoded onto Inst::Vop3, verified against the encoding rather than assumed
- [x] #2 The recompiler applies it (clamp the result to [0,1] for float results), and the interpreter agrees, so the differential harness stays meaningful
- [x] #3 The disassembler prints it, matching the omod/neg/abs work from task-182 — the text must not misrepresent the instruction
- [x] #4 A test covers clamp alone and clamp combined with omod, since both are output modifiers and their ORDER matters
- [x] #5 Confirmed with the real decoder whether Celeste's shader corpus actually sets the bit, and recorded either way
- [x] #6 build + cargo test + clippy clean
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Encoding: clamp = LOW-dword bit 11 on GFX7 (GFX8 moved it to bit 15). Verified with llvm-mc -mcpu=bonaire: 'v_mad_f32 v0,v1,v2,v3 clamp' -> [0xd2820800, 0x040e0501]; adding mul:2 only changes the HIGH dword, so clamp and omod are independent fields.

Trap 1 (ordering) REAL: hardware chain is raw -> omod -> clamp. Both backends apply apply_omod then apply_clamp. Mutation-tested: reversing the order in both backends makes vop3_clamp_ps fail with [1.0,0.0,1.5,0.5] vs expected [1.0,0.0,1.0,0.75].

Trap 2 (float vs integer) NOT REAL on this GPU. llvm-mc -mcpu=bonaire rejects 'v_mad_u32_u24 ... clamp' with 'integer clamping is not supported on this GPU' — GFX7 has no integer clamp at all, so there is no signed-saturation meaning to implement. No dead code written; the int path (v_mad_u32_u24) carries a comment saying why. v_cvt_pkrtz_f16_f32 is the only non-uop VOP3 that legally accepts clamp; corpus does not use it, left unhandled.

Trap 3 (NaN) REAL but self-resolving. GCN clamp is min/max-based: max(NaN,0)=0 -> NaN saturates to 0. Lowered as f_min(f_max(v,0),1) using the SAME AluBuilder primitives both backends already use for v_min/v_max (host f32::min/max, GLSL FMin/FMax) rather than GLSL FClamp — FClamp is DEFINED as that composition, and reusing the audited pair means no new divergence surface. Locked by corpus shader vop3_clamp_nan_ps.

AC #5 SETTLED — the crude scan was a TRUE POSITIVE. Real decoder over gpu-snapshots/shaders/*.sb: exactly one clamp in the corpus, ps-27883c9d7c88cd30.sb @dw25 'v_mac_f32 v7, s1, v0, s0 clamp'. Alignment is sound (stream ends s_endpgm at dw40 = 41 dwords = 164 bytes = file size). It is a LIVE symptom, not prophylactic: v7 = 0.0740*dist + (-0.3333) saturated, then consumed by three v_mac as a LERP FACTOR (v2 += (s12-v2)*v7 etc). Unclamped, v7 goes negative for small dist and EXTRAPOLATES the color past the texture sample instead of holding at it.

Paths changed: decoder.rs (extract bit 11), inst.rs (clamp: bool on Inst::Vop3), uop.rs (shared apply_clamp), interp.rs + recompile.rs (apply after omod), disasm.rs (print ' clamp' BEFORE mul:2/div:2 — llvm-mc's VOP3 asm string is ...$clamp$omod, i.e. text order is the reverse of application order).

Tests: decode.rs::vop3_carries_clamp (bit position, independence from omod, clear case); disasm.rs::renders_vop3_clamp_and_clamp_with_omod (text incl. clamp+abs/neg+div:2); corpus vop3_clamp_ps (clamp alone from above/below + two omod ORDER probes chosen so the reversed chain gives a different value) and vop3_clamp_nan_ps (NaN/+inf/-inf). Both corpus shaders run through the differential harness — interp oracle vs analytic math AND CPU-SPIR-V vs oracle bit-for-bit. Mutation-tested that dropping clamp in the recompiler alone is caught (asymmetry would not slip through).

Gates: cargo build --release, cargo test --workspace (513 passed / 0 failed, was 511), clippy -D warnings, cargo fmt — all clean. NOT COMMITTED (maintainer commits).

Side notes, out of scope: (1) regen.sh regenerates cbranch_select_ps.code.bin differently from what is committed (84 -> 80 bytes) with this llvm version — pre-existing drift, reverted, not touched. (2) disasm prints a spurious third source on 2-source VOP3 forms (e.g. 'v_mac_f32 v7, s1, v0, s0', 'v_cvt_pkrtz_f16_f32 v0, v4, 1.0, s0') — pre-existing, same family as task-182, not in this task's scope.
<!-- SECTION:NOTES:END -->
