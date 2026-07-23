; loop_accum_ps — GFX7 / Sea Islands (bonaire) counted LOOP (task-129, loops slice).
;
; The first LOOPING corpus shader: a small counted loop with a tiny fixed, UNIFORM
; trip count that accumulates a constant into a VGPR each iteration, then exports the
; accumulated value. This is the structured-loop shape a recompiler must lower to a
; SPIR-V OpLoopMerge (back-edge to the loop header, single exit at the merge) — the
; last control-flow slice after the forward `if` (cbranch_alpha_ps) and if-else
; diamond (cbranch_select_ps).
;
; It exists to validate the loop lowering end to end: the CFG recognizes the back-edge
; (a branch target at a LOWER dword than the branch) as a natural loop; the recompiler
; emits OpLoopMerge %merge %continue None with the back-edge conditional as an
; OpBranchConditional (continue-vs-merge), carrying the loop variables across
; iterations through Function OpVariable load/store with NO OpPhi (glslang's
; pre-mem2reg form, which spirv-val accepts); and the interp oracle walks the body
; under EXEC, re-evaluating the back-edge condition each iteration and dropping a lane
; from EXEC when it fails the continue test, exiting when EXEC ∩ body == 0.
;
; Control flow (deterministic constant inputs; the loop is UNIFORM — every lane runs
; exactly 4 iterations — so interp and recompile agree bit-for-bit):
;
;   v0 = 0.0                       ; accumulator
;   v1 = 0.0                       ; loop counter
;   v2 = 1.0                       ; export alpha (a VGPR — exp takes no immediates)
;   v3 = 4.0                       ; loop limit (a VGPR — VOPC e32 needs a reg src1)
; header:
;   v0 = v0 + 0.25                 ; accumulate a constant each iteration
;   v1 = v1 + 1.0                  ; ++counter
;   v_cmp_lt_f32 vcc, v1, v3       ; continue while counter < 4 (UNIFORM: same for all lanes)
;   s_cbranch_vccnz header         ; BACK-EDGE: loop while VCC != 0. Target < branch → loop.
;   ; --- exit (merge) ---------------------------------------------------
;   exp mrt0, v0, v0, v0, v2       ; v0 accumulated 4 * 0.25 = 1.0, alpha 1.0
;   s_endpgm
;
; Trip count: counter goes 1,2,3,4; `1<4,2<4,3<4` continue, `4<4` false → exit after
; the 4th iteration. Accumulator = 4 * 0.25 = 1.0 exactly. Export (1.0, 1.0, 1.0, 1.0),
; every value exact in f32. The differential spec pins this; the interp oracle models
; the loop by re-running the header body under EXEC and re-testing the back-edge each
; iteration (with a safety iteration cap); the CPU SPIR-V value oracle re-executes the
; OpLoopMerge/back-edge (with a block-visit cap). All three must agree.
;
; Self-authored corpus shader (task-129); ZERO copyrighted assets. Regenerate the
; OrbShdr blob: crates/gcn/tests/corpus/regen.sh then the corpus.rs
; --ignored regen_sb_blobs test.

	v_mov_b32 v0, 0
	v_mov_b32 v1, 0
	v_mov_b32 v2, 1.0
	v_mov_b32 v3, 4.0

header:                                 ; dword offset 4 (the back-edge target)
	v_add_f32 v0, 0x3e800000, v0    ; v0 += 0.25  (2 dwords: opcode + 0x3e800000 literal)
	v_add_f32 v1, 1.0, v1           ; v1 += 1.0
	v_cmp_lt_f32 vcc, v1, v3

	; Loop back to the header while the counter is still < 4 (VCC != 0). The branch is
	; at dword 8; target = 8 + 1 + simm16 = 4 (header) ⇒ simm16 = -5 = 0xFFFB. The
	; corpus uses a numeric simm16 (not a label) so llvm-mc resolves it inline — a label
	; leaves an unresolved relocation the byte-extraction in regen.sh cannot pack.
	s_cbranch_vccnz 0xfffb          ; back-edge to `header`

	exp mrt0, v0, v0, v0, v2 done vm
	s_endpgm
