; cbuffer16_vs — GFX7 / Sea Islands (bonaire) vertex shader that loads a 16-dword
; constant buffer with `s_buffer_load_dwordx16` (task-113.4.2 — the op the retail
; Celeste VS emit right after their fetch-shader call to load their 4×4 transform
; matrix). Self-authored corpus; ZERO copyrighted assets.
;
; It loads a 16-dword constant block (s[0:15]) from the V# descriptor in s[4:7],
; then exports the block's DIAGONAL elements (s0, s5, s10, s15) as pos0 and param0.
; Reading exactly the four corners of the 16-dword load proves both the oracle and
; the recompiler address the full dwordx16 extent at the right dword offsets (0, 5,
; 10, 15) — a wider load than the dwordx4 cbuffer_ps already exercised.
;
; ABI:
;   s[4:7] : 128-bit V# descriptor for the constant buffer (user data)
;   s[0:15]: the loaded 16-dword constant block (4×4 matrix's worth)
;
; The exported diagonal values are read straight off the constant bytes the
; differential spec places in the mock buffer — analytic-exact (the four f32s at
; dword 0/5/10/15), independent of any float rounding.
;
; Regenerate the raw bytes with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding cbuffer16_vs.s
; (see crates/gcn/tests/corpus/regen.sh).

	s_mov_b32 vcc_hi, 0x1
	s_buffer_load_dwordx16 s[0:15], s[4:7], 0x0
	s_waitcnt lgkmcnt(0)
	v_mov_b32 v0, s0
	v_mov_b32 v1, s5
	v_mov_b32 v2, s10
	v_mov_b32 v3, s15
	exp pos0, v0, v1, v2, v3 done
	exp param0, v0, v1, v2, v3
	s_endpgm
