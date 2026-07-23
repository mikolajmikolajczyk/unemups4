; cmp_cndmask_ps — GFX7 / Sea Islands (bonaire) standalone VOPC + v_cndmask_b32.
;
; Exercises the predication/VCC family the retail pixel shaders reach (sh11/12 use
; v_cmp_lt_f32 → VCC, sh16 uses v_cmp_gt_f32 → VCC; a v_cndmask_b32 then consumes the
; predicate). Per-invocation SPIR-V is one lane, so VCC is a single bool: the compare
; emits a normal SPIR-V comparison, v_cndmask lowers to OpSelect.
;
;   ch0: v_cmp_lt_f32 vcc, 1.0, 2.0  → true;  cndmask(0.25, 0.75) → 0.75
;   ch1: v_cmp_gt_f32 vcc, 1.0, 2.0  → false; cndmask(0.25, 0.75) → 0.25
;   ch2: constant 0.5
;   ch3: constant 1.0
; So the export is (0.75, 0.25, 0.5, 1.0) — every value exact in f32, so the analytic
; expectation is independent of the compare/select under test. Self-authored corpus
; shader (task-113.4.2); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob: crates/gcn/tests/corpus/regen.sh then the corpus.rs
; --ignored regen_sb_blobs test.

	; ch0: 1.0 < 2.0 (true) → select the true source (0.75)
	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 2.0
	v_cmp_lt_f32 vcc, v0, v1
	v_mov_b32 v2, 0.25
	v_mov_b32 v3, 0.75
	v_cndmask_b32 v4, v2, v3, vcc

	; ch1: 1.0 > 2.0 (false) → select the false source (0.25)
	v_cmp_gt_f32 vcc, v0, v1
	v_cndmask_b32 v5, v2, v3, vcc

	v_mov_b32 v6, 0.5
	v_mov_b32 v7, 1.0
	exp mrt0, v4, v5, v6, v7 done vm
	s_endpgm
