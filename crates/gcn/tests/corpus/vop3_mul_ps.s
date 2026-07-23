; vop3_mul_ps — GFX7 / Sea Islands (bonaire) v_mul_f32 re-encoded as VOP3.
;
; Exercises v_mul_f32_e64 (VOP3 op 0x108 = VOP2 0x08 + 0x100), which real Celeste
; pixel shaders reach to carry an abs source modifier the VOP2 encoding cannot. Here
; v2 = v0 * |v1| = 4.0 * |-0.5| = 2.0 (exact in f32), so the analytic expectation is
; exact. The abs:2 modifier (abs on src1) matches the retail shape (sh17). Self-authored
; corpus shader (task-113.4.2); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding vop3_mul_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 4.0
	v_mov_b32 v1, -0.5
	v_mul_f32_e64 v2, v0, |v1|
	v_mov_b32 v3, 1.0
	exp mrt0, v2, v2, v2, v3 done vm
	s_endpgm
