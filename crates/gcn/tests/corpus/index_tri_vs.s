; index_tri_vs — GFX7 / Sea Islands (bonaire) index-derived vertex shader.
;
; A vertex shader with NO vertex buffer and NO fetch shader: it derives clip-space
; position purely from the launch vertex index in v0, the idiom a full-screen fill
; draw uses. This is the shape that pins the launch ABI itself — v0 IS the vertex
; index, read here as a plain ALU source rather than through an `idxen` fetch
; (task-184). Self-authored corpus shader; ZERO copyrighted assets.
;
; ABI:
;   v0 : vertex index (read directly, no fetch)
;
; Math (vertex 0/1/2 ⇒ the three corners of a screen-covering triangle):
;   x = f32(idx & 1) * 2.0 - 1.0    ⇒ -1, +1, -1
;   y = f32(idx & ~1) - 1.0         ⇒ -1, -1, +1
;   z = 0.0, w = 1.0
;
; Regenerate the raw code bytes with crates/gcn/tests/corpus/regen.sh (see that
; script and the corpus.rs module note).

	v_and_b32 v1, 1, v0
	v_cvt_f32_u32 v1, v1
	v_mad_f32 v1, v1, 2.0, -1.0
	v_and_b32 v2, -2, v0
	v_cvt_f32_u32 v2, v2
	v_add_f32 v2, -1.0, v2
	v_mov_b32 v3, 0
	v_mov_b32 v4, 1.0
	exp pos0, v1, v2, v3, v4 done
	exp param0, v1, v2, v3, v4
	s_endpgm
