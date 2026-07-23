; vop3_mad_sin_fract_ps — GFX7 / Sea Islands (bonaire) VOP3 + VOP1 mechanical batch.
;
; Exercises three ops real Celeste retail shaders reach that were the recompiler's
; first-wall after the min/max/shift batch:
;   v_sin_f32          (VOP1 op 0x35): GCN sine is sin(2*PI*S0) — arg in revolutions.
;                      sin(2*PI*0.25) = sin(PI/2) = 1.0 (exact).
;   v_fract_f32_e64    (VOP3 op 0x1A0): fract re-encoded as VOP3 to carry the abs src
;                      modifier and the omod output scale. fract(|-2.125|) = 0.125,
;                      then mul:4 (omod) -> 0.5 (exact).
;   v_mad_u32_u24      (VOP3 op 0x143): 24-bit unsigned integer multiply-add,
;                      (3 & 0xFFFFFF)*(2 & 0xFFFFFF)+1 = 7 -> cvt_f32_u32 -> 7.0.
;
; Every result is exact in f32, so the analytic expectation is exact. Self-authored
; corpus shader (task-113.4.2); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding vop3_mad_sin_fract_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 0.25
	v_sin_f32 v0, v0
	v_mov_b32 v1, -2.125
	v_fract_f32_e64 v1, |v1| mul:4
	v_mov_b32 v2, 3
	v_mad_u32_u24 v2, v2, 2, 1
	v_cvt_f32_u32 v2, v2
	v_mov_b32 v3, 1.0
	exp mrt0, v0, v1, v2, v3 done vm
	s_endpgm
