; vop3_mac_ps — GFX7 / Sea Islands (bonaire) v_mac_f32 multiply-accumulate.
;
; Exercises v_mac_f32_e64 (VOP3 op 0x11F = VOP2 0x1F + 0x100), which real Celeste
; pixel shaders reach (sh06/sh08/sh17). The dst is an implicit accumulator:
; D = S0*S1 + D. v2 is pre-loaded to 0.5, then v2 = 2.0*1.5 + 0.5 = 3.5 (exact in
; f32), so the analytic expectation is exact. UNFUSED on GCN (the multiply rounds,
; then the add rounds), so both oracle and recompiler do a*b + d as two roundings, not
; a fused mul-add. Self-authored corpus shader (task-113.4.2); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding vop3_mac_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v2, 0.5
	v_mov_b32 v0, 2.0
	v_mov_b32 v1, 1.5
	v_mac_f32_e64 v2, v0, v1
	v_mov_b32 v3, 1.0
	exp mrt0, v2, v2, v2, v3 done vm
	s_endpgm
