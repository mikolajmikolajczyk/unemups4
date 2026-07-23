; cbranch_select_ps — GFX7 / Sea Islands (bonaire) if-ELSE diamond.
;
; The second branching corpus shader (task-129, slice 4): a real if-else DIAMOND —
; a conditional branch whose TWO arms each write a distinct color to the SAME output
; VGPRs and both reconverge at a common merge block that exports. This is the shape
; retail pixel shaders reach for a two-way select (e.g. a lit vs unlit path, or a
; branch selecting between two quality tiers) — unlike the single forward `if` of
; cbranch_alpha_ps (where one side is a no-op skip), here BOTH sides do work.
;
; It exists to validate the load/store register model's last-writer-wins across a
; REAL merge (the no-phi decision): both arms OpStore the same VGPR; only the taken
; lane's arm survives, and the recompiler carries it to the export with NO OpPhi.
;
; Control flow (deterministic constant inputs, every value exact in f32):
;
;   v0 = 1.0, v1 = 2.0
;   v_cmp_lt_f32 vcc, v0, v1     ; 1.0 < 2.0 → TRUE → VCC bit set (vcc != 0)
;   s_cbranch_vccz arm_dark      ; vccz taken only when VCC == 0. Here VCC != 0, so
;                                ; NOT taken: control falls into arm_bright below.
;   ; --- arm_bright (fall, the alpha test PASSED) -----------------------
;   v2 = 0.75 ; v3 = 0.5 ; v4 = 0.25 ; v5 = 1.0
;   s_branch merge               ; skip the dark arm
;   ; --- arm_dark (taken, the alpha test FAILED) ------------------------
;   v2 = 0.125 ; v3 = 0.125 ; v4 = 0.125 ; v5 = 1.0
;   ; --- merge ----------------------------------------------------------
;   exp mrt0, v2, v3, v4, v5 done vm
;
; Because VCC is set (1.0 < 2.0), the BRIGHT arm runs and the export is
;   (0.75, 0.5, 0.25, 1.0)   — the dark arm's writes to v2..v5 are overwritten /
;                              never applied for the live lane (last-writer-wins).
; The differential spec pins this; the interp oracle models it by running each arm
; under its EXEC lane mask and reconverging at the merge, and the CPU SPIR-V value
; oracle re-executes the OpSelectionMerge/OpBranchConditional diamond — both must
; agree, validating that no OpPhi is needed.
;
; Self-authored corpus shader (task-129); ZERO copyrighted assets. Regenerate the
; OrbShdr blob: crates/gcn/tests/corpus/regen.sh then the corpus.rs
; --ignored regen_sb_blobs test.

	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 2.0
	v_cmp_lt_f32 vcc, v0, v1

	; take the dark arm when the test fails (VCC == 0); otherwise fall into bright.
	s_cbranch_vccz arm_dark

arm_bright:
	v_mov_b32 v2, 0x3f400000        ; 0.75
	v_mov_b32 v3, 0.5
	v_mov_b32 v4, 0x3e800000        ; 0.25
	v_mov_b32 v5, 1.0
	s_branch merge

arm_dark:
	v_mov_b32 v2, 0x3e000000        ; 0.125
	v_mov_b32 v3, 0x3e000000        ; 0.125
	v_mov_b32 v4, 0x3e000000        ; 0.125
	v_mov_b32 v5, 1.0

merge:
	exp mrt0, v2, v3, v4, v5 done vm
	s_endpgm
