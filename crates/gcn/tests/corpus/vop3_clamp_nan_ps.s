; vop3_clamp_nan_ps — GFX7 / Sea Islands (bonaire) VOP3 `clamp` on NaN / ±inf (task-188).
;
; GCN's clamp is a MIN/MAX-based saturate, not a passthrough, so it is defined on the
; non-finite inputs and in particular turns NaN into 0.0 — `max(NaN, 0.0)` returns the
; non-NaN operand, then `min(0.0, 1.0)` is 0.0. Both backends lower clamp through the
; same `f_max`/`f_min` pair (host `f32::max`/`f32::min` in the interpreter, GLSL
; `FMax`/`FMin` in the recompiler), so this shader pins that they agree bit-for-bit
; here rather than diverging only on the exotic inputs the differential harness would
; then report as a mysterious failure.
;
;   r: NaN  -> clamp -> 0.0
;   g: +inf -> clamp -> 1.0
;   b: -inf -> clamp -> 0.0
;   a: 1.0  (constant, no clamp)
;
; The non-finite values come in via v_mov_b32 literals: VOP3 has no 32-bit literal
; operand on GFX7, so the `v_mad_f32 ... clamp` reads them from VGPRs.
; Self-authored corpus shader (task-188); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding vop3_clamp_nan_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v2, 0x7fc00000
	v_mov_b32 v3, 0x7f800000
	v_mov_b32 v4, 0xff800000
	v_mad_f32 v2, v2, 1.0, 0 clamp
	v_mad_f32 v3, v3, 1.0, 0 clamp
	v_mad_f32 v4, v4, 1.0, 0 clamp
	v_mov_b32 v5, 1.0
	exp mrt0, v2, v3, v4, v5 done vm
	s_endpgm
