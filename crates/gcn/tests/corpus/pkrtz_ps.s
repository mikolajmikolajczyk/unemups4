; pkrtz_ps — GFX7 / Sea Islands (bonaire) f16-packed compressed-export pixel shader.
;
; Moves a constant RGBA into v0..v3, packs it to two f16 pairs with
; v_cvt_pkrtz_f16_f32 (the pack RETAIL managed-runtime pixel shaders use before a
; compressed MRT export), then exports it with `compr`: vsrc0 (v0) carries channels
; 0,1 and vsrc1 (v1) carries channels 2,3, two f16 per register. Self-authored
; corpus shader (task-113.4.2 AC#3/#6); ZERO copyrighted assets.
;
; The constants are all exactly representable in f16 (1.0, 0.25, 0.5, 1.0), so the
; f16 round-trip is lossless and the analytic expectation equals the literals — the
; test isolates the pack/unpack PLUMBING from f16 rounding (which is `half`'s own
; concern).
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding pkrtz_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 0.25
	v_mov_b32 v2, 0.5
	v_mov_b32 v3, 1.0
	v_cvt_pkrtz_f16_f32 v0, v0, v1
	v_cvt_pkrtz_f16_f32 v1, v2, v3
	exp mrt0, v0, v0, v1, v1 done compr vm
	s_endpgm
