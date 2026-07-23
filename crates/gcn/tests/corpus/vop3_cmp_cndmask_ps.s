; vop3_cmp_cndmask_ps — GFX7 / Sea Islands (bonaire) VOP3-form VOPC + v_cndmask.
;
; Exercises the VOP3-encoded predication the retail pixel shaders reach (sh02 uses
; v_cmp_lt_f32 → s[16:17], sh15 uses v_cmp_gt_f32 → s[12:13]). Unlike the standalone
; VOPC (which writes the implicit VCC), the VOP3 form writes the compare bool to an
; ARBITRARY SGPR pair named by the `sdst` field, and the VOP3-form v_cndmask reads
; that pair as its predicate (src2). Per-invocation SPIR-V collapses each SGPR-pair
; mask to a single bool.
;
;   ch0: v_cmp_lt_f32 s[16:17], 1.0, 2.0 → true;  cndmask(0.25,0.75) via s[16:17] → 0.75
;   ch1: v_cmp_gt_f32 s[12:13], 1.0, 2.0 → false; cndmask(0.25,0.75) via s[12:13] → 0.25
;   ch2: 0.5 ; ch3: 1.0
; Export (0.75, 0.25, 0.5, 1.0) — every value exact in f32; the expectation is
; independent of the compare/select under test. Self-authored (task-113.4.2); ZERO
; copyrighted assets.
;
; Regenerate via crates/gcn/tests/corpus/regen.sh then the corpus.rs regen_sb_blobs test.

	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 2.0
	v_mov_b32 v2, 0.25
	v_mov_b32 v3, 0.75

	; ch0: 1.0 < 2.0 (true) into s[16:17]; select the true source (0.75)
	v_cmp_lt_f32 s[16:17], v0, v1
	v_cndmask_b32 v4, v2, v3, s[16:17]

	; ch1: 1.0 > 2.0 (false) into s[12:13]; select the false source (0.25)
	v_cmp_gt_f32 s[12:13], v0, v1
	v_cndmask_b32 v5, v2, v3, s[12:13]

	v_mov_b32 v6, 0.5
	v_mov_b32 v7, 1.0
	exp mrt0, v4, v5, v6, v7 done vm
	s_endpgm
