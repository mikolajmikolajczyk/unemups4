; minmax_shift_ps — GFX7 / Sea Islands (bonaire) VOP2 min/max + logical-shift shader.
;
; Exercises v_min_f32, v_max_f32 (float clamp ops) and v_lshrrev_b32 (unsigned logical
; shift right, reversed operands). min(0.5,0.25)=0.25, max=0.5; 8>>1=4 then cvt to 4.0
; — every result exact in f32, so the analytic expectation is exact. Self-authored
; corpus shader (task-113.4.2); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding minmax_shift_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 0.5
	v_mov_b32 v1, 0.25
	v_min_f32 v2, v0, v1
	v_max_f32 v3, v0, v1
	v_mov_b32 v4, 8
	v_lshrrev_b32 v4, 1, v4
	v_cvt_f32_u32 v4, v4
	exp mrt0, v2, v3, v4, v4 done vm
	s_endpgm
