; rcp_ps — GFX7 / Sea Islands (bonaire) v_rcp_f32 reciprocal pixel shader.
;
; Exercises v_rcp_f32 (VOP1 op 0x2A), the reciprocal retail pixel shaders reach (e.g.
; sh17, for a perspective/normalize divide). 1/4=0.25 and 1/2=0.5 are exact in f32, so
; the analytic expectation is exact. GCN's v_rcp is an approximate macro on hardware;
; the oracle and the recompiler both model the exact 1.0/x (the oracle divides, the
; recompiler emits OpFDiv 1.0/x — GLSL.std.450 has no reciprocal), so they agree
; bit-for-bit — the sub-ULP deviation from hardware is invisible through the RT.
; Self-authored corpus shader (task-113.4.2); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding rcp_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 4.0
	v_rcp_f32 v0, v0
	v_mov_b32 v1, 2.0
	v_rcp_f32 v1, v1
	v_mov_b32 v3, 1.0
	exp mrt0, v0, v1, v0, v3 done vm
	s_endpgm
