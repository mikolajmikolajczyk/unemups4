; vop3_pkrtz_ps — GFX7 / Sea Islands (bonaire) v_cvt_pkrtz_f16_f32 in VOP3 form.
;
; Same f16 pack + compressed-export plumbing as pkrtz_ps, but the pack is encoded as
; VOP3 (op 0x12F = VOP2 0x2F + 0x100) — the form retail pixel shaders reach when an
; inline/abs operand forces e64 (sh06/sh08). The constants are all exactly
; representable in f16 (1.0, 0.25, 0.5, 1.0), so the f16 round-trip is lossless and
; the analytic expectation equals the literals — isolating the VOP3 pack path from
; f16 rounding. Self-authored corpus shader (task-113.4.2); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding vop3_pkrtz_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 0.25
	v_mov_b32 v2, 0.5
	v_mov_b32 v3, 1.0
	v_cvt_pkrtz_f16_f32_e64 v0, v0, v1
	v_cvt_pkrtz_f16_f32_e64 v1, v2, v3
	exp mrt0, v0, v0, v1, v1 done compr vm
	s_endpgm
