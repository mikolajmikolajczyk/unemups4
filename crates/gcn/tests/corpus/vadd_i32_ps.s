; vadd_i32_ps — GFX7 / Sea Islands (bonaire) v_add_i32 with a VCC carry-OUT.
;
; Exercises v_add_i32 (VOP2 op 0x25, sh01): an integer add whose 32-bit result is a
; plain wrapping add and whose unsigned-overflow carry goes to VCC. The carry only
; matters if a later op reads it — here two v_cndmask_b32 consume it, proving both the
; wrapping result AND the carry bit are modeled.
;
;   v_add_i32 v2, vcc, 1, 2         → sum = 3, carry = false
;   v_cvt_f32_i32 → 3.0                                              (ch0)
;   v_cndmask(0.25, 0.75) via VCC   → false → 0.25                   (ch1)
;   v_add_i32 v5, vcc, -1, 1        → sum = 0 (wraps), carry = true
;   v_cndmask(0.25, 0.75) via VCC   → true  → 0.75                   (ch2)
;   ch3: 1.0
; Export (3.0, 0.25, 0.75, 1.0) — every value exact in f32, so the expectation is
; independent of the add/carry under test. Self-authored (task-113.4.2); ZERO
; copyrighted assets.
;
; Regenerate via crates/gcn/tests/corpus/regen.sh then the corpus.rs regen_sb_blobs test.

	v_mov_b32 v0, 1
	v_mov_b32 v1, 2
	v_mov_b32 v6, 0.25
	v_mov_b32 v7, 0.75

	; no-carry add: 1 + 2 = 3, VCC carry = 0
	v_add_i32 v2, vcc, v0, v1
	v_cvt_f32_i32 v10, v2
	v_cndmask_b32 v8, v6, v7, vcc

	; carry add: 0xFFFFFFFF + 1 = 0 (wraps), VCC carry = 1
	v_mov_b32 v3, -1
	v_mov_b32 v4, 1
	v_add_i32 v5, vcc, v3, v4
	v_cndmask_b32 v9, v6, v7, vcc

	v_mov_b32 v11, 1.0
	exp mrt0, v10, v8, v9, v11 done vm
	s_endpgm
