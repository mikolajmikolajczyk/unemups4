; index_tri_inplace_vs — GFX7 / Sea Islands (bonaire) index-derived vertex shader
; that CLOBBERS its own launch-index register in place.
;
; Same job as index_tri_vs (derive clip-space position from the launch vertex index
; in v0, no vertex buffer and no fetch shader), but written the way a real full-screen
; fill shader is: the second read of v0 is the SOURCE of an instruction whose
; DESTINATION is also v0.
;
;   v_and_b32 v1, 1, v0     ; first read of v0  — dst != src
;   v_and_b32 v0, -2, v0    ; second read of v0 — dst == src, IN PLACE
;
; That in-place shape is what index_tri_vs does not cover: every ALU emitter untracks
; the destination as a launch-index carrier BEFORE evaluating its source operands, so
; without spilling the index into the register slot first the second read sees the
; zero initializer and the Y coordinate of every vertex collapses onto -1 — a zero-area
; triangle that rasterizes nothing (task-184).
;
; ABI:
;   v0 : vertex index (read directly, no fetch)
;
; Math (integer, mirroring the shape retail fill shaders use):
;   x = f32_i32((idx & 1) * 2 - 1)   ⇒ -1, +1 by index parity
;   y = f32_i32((idx & ~1) - 1)      ⇒ -1 for idx 0/1, +1 for idx 2/3
;   z = 0.0, w = 1.0
;
; Regenerate the raw code bytes with crates/gcn/tests/corpus/regen.sh (see that
; script and the corpus.rs module note).
;
; Self-authored corpus shader; ZERO copyrighted assets.

	v_and_b32 v1, 1, v0
	v_and_b32 v0, -2, v0
	v_mad_u32_u24 v1, v1, 2, -1
	v_add_i32 v2, vcc, -1, v0
	v_cvt_f32_i32 v0, v1
	v_cvt_f32_i32 v1, v2
	v_mov_b32 v2, 0
	v_mov_b32 v3, 1.0
	exp pos0, v0, v1, v2, v3 done
	exp param0, v0, v1, v2, v3
	s_endpgm
