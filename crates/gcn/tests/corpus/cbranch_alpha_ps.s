; cbranch_alpha_ps — GFX7 / Sea Islands (bonaire) forward conditional branch.
;
; The first branching corpus shader (task-129 first slice): a single forward `if`
; built from v_cmp_lt_f32 → VCC followed by s_cbranch_vccz. This is the shape retail
; pixel shaders reach for an alpha/quality-tier gate — a compare produces VCC, a
; conditional branch skips a block of compute+export when the wave condition holds.
;
; Per-invocation SPIR-V is one lane, so VCC is a single bool and the whole-wave
; branch degenerates to a per-lane scalar `if` the driver reconverges. The CPU oracle
; models the same control flow by narrowing EXEC at the branch and OR-restoring it at
; the structured merge (the post-dominator = the branch target).
;
; Control flow (deterministic constant inputs, every value exact in f32):
;
;   v0 = 1.0, v1 = 2.0
;   v_cmp_lt_f32 vcc, v0, v1     ; 1.0 < 2.0 → TRUE → VCC bit set (vcc != 0)
;   ; pre-seed the export channels with the "background" color 0.25
;   v2..v5 = 0.25
;   s_cbranch_vccz <merge>       ; vccz taken only when VCC == 0. Here VCC != 0, so
;                                ; the branch is NOT taken: control falls into the
;                                ; "bright" block below.
;   ; --- fall block (taken when the alpha test passes) ------------------
;   v2 = 0.75   ; overwrite R
;   v3 = 0.50   ; overwrite G
;   v4 = 0.25   ; B stays 0.25
;   v5 = 1.0    ; overwrite A
;   ; --- merge -----------------------------------------------------------
;   exp mrt0, v2, v3, v4, v5 done vm
;
; Because VCC is set (1.0 < 2.0), the fall block runs and the export is
;   (0.75, 0.50, 0.25, 1.0).
; If the compare were false the branch would skip the fall block and the export would
; be the pre-seeded (0.25, 0.25, 0.25, 0.25). The differential spec pins the taken
; path; the recompiler and the interp oracle must agree on it.
;
; Self-authored corpus shader (task-129); ZERO copyrighted assets. Regenerate the
; OrbShdr blob: crates/gcn/tests/corpus/regen.sh then the corpus.rs
; --ignored regen_sb_blobs test.

	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 2.0
	v_cmp_lt_f32 vcc, v0, v1

	; pre-seed the export channels with the background color (0.25)
	v_mov_b32 v2, 0.25
	v_mov_b32 v3, 0.25
	v_mov_b32 v4, 0.25
	v_mov_b32 v5, 0.25

	; skip the bright block when the alpha test fails (VCC == 0). The displacement
	; (6 dwords) lands on the `exp` at the merge: branch is at dword 11, so the target
	; is 11 + 1 + 6 = 18 = the exp instruction (the four bright v_mov's span dwords
	; 12..17 and are skipped when the branch is taken).
	s_cbranch_vccz 6

	; bright block — overwrite the color when the test passes
	v_mov_b32 v2, 0.75
	v_mov_b32 v3, 0.5
	v_mov_b32 v4, 0.25
	v_mov_b32 v5, 1.0

	; merge / export
	exp mrt0, v2, v3, v4, v5 done vm
	s_endpgm
