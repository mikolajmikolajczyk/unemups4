; vop3_cmp3_ps — GFX7 / Sea Islands (bonaire) VOP3-form v_cmp_le/ge/eq_f32.
;
; Companion to vop3_cmp_cndmask_ps (which covers VOP3-form lt/gt). This one
; exercises the remaining three VOP3-encoded f32 compares — le, ge, eq — each
; writing its per-lane bool to an ARBITRARY SGPR pair (the `sdst` field) rather
; than the implicit VCC of the standalone VOPC. A VOP3-form v_cndmask reads each
; pair as its predicate (src2) so the compare result is observable in the export.
; Per-invocation SPIR-V collapses each SGPR-pair mask to a single bool.
;
;   ch0: v_cmp_le_f32 s[16:17], 1.0, 2.0 → (1.0 <= 2.0)=true;  cndmask(0.25,0.75) → 0.75
;   ch1: v_cmp_ge_f32 s[12:13], 1.0, 2.0 → (1.0 >= 2.0)=false; cndmask(0.25,0.75) → 0.25
;   ch2: v_cmp_eq_f32 s[8:9],  1.0, 1.0 → (1.0 == 1.0)=true;  cndmask(0.25,0.75) → 0.75
;   ch3: 1.0
; Export (0.75, 0.25, 0.75, 1.0) — every value exact in f32; the expectation is
; reasoned from the compare truth values, independent of the compare/select under
; test. Self-authored (task-124); ZERO copyrighted assets.
;
; Regenerate via crates/gcn/tests/corpus/regen.sh then the corpus.rs regen_sb_blobs test.

	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 2.0
	v_mov_b32 v2, 0.25
	v_mov_b32 v3, 0.75

	; ch0: 1.0 <= 2.0 (true) into s[16:17]; select the true source (0.75)
	v_cmp_le_f32 s[16:17], v0, v1
	v_cndmask_b32 v4, v2, v3, s[16:17]

	; ch1: 1.0 >= 2.0 (false) into s[12:13]; select the false source (0.25)
	v_cmp_ge_f32 s[12:13], v0, v1
	v_cndmask_b32 v5, v2, v3, s[12:13]

	; ch2: 1.0 == 1.0 (true) into s[8:9]; select the true source (0.75)
	v_cmp_eq_f32 s[8:9], v0, v0
	v_cndmask_b32 v6, v2, v3, s[8:9]

	v_mov_b32 v7, 1.0
	exp mrt0, v4, v5, v6, v7 done vm
	s_endpgm
