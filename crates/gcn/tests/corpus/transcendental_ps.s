; transcendental_ps — GFX7 / Sea Islands (bonaire) VOP1 transcendental pixel shader.
;
; Exercises the single-input float ops retail pixel shaders use for shaping:
; v_floor_f32, v_fract_f32, v_sqrt_f32. Constants are chosen so every result is exact
; in f32 (floor(2.5)=2.0, fract(2.5)=0.5, sqrt(4.0)=2.0), so the analytic expectation
; is exact. Self-authored corpus shader (task-113.4.2); ZERO copyrighted assets.
;
; GCN's v_sqrt is an approximate macro on real hardware; the oracle and the recompiler
; both model the correctly-rounded IEEE Sqrt (GLSL.std.450 Sqrt), so they agree
; bit-for-bit — the sub-ULP deviation from hardware is invisible through the RT.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding transcendental_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 2.5
	v_floor_f32 v1, v0
	v_fract_f32 v2, v0
	v_mov_b32 v3, 4.0
	v_sqrt_f32 v3, v3
	exp mrt0, v1, v2, v3, v3 done vm
	s_endpgm
