; texture_sample_ps — GFX7 / Sea Islands (bonaire) textured pixel shader.
;
; Interpolates a per-vertex UV varying (attr0.xy) with the two-phase GCN
; interpolation, samples a 2D texture (image_sample) through a T# (s[0:7]) + S#
; (s[8:11]), and exports the sampled RGBA to render target 0 (mrt0). The classic
; textured-quad PS. Self-authored corpus shader (task-55); ZERO copyrighted assets.
;
; ABI:
;   s0        : interpolation parameter base (loaded into m0 before v_interp_*)
;   s[0:7]    : T# image resource (256-bit)      s[8:11] : S# sampler (128-bit)
;   v0        : barycentric I, v1 : barycentric J (SPI-provided PS inputs)
;
; Regenerate the OrbShdr blob with regen.sh (llvm-mc) then the corpus.rs builder.

	s_mov_b32 m0, s0
	v_interp_p1_f32 v2, v0, attr0.x
	v_interp_p2_f32 v2, v1, attr0.x
	v_interp_p1_f32 v3, v0, attr0.y
	v_interp_p2_f32 v3, v1, attr0.y
	image_sample v[4:7], v[2:3], s[0:7], s[8:11] dmask:0xf
	exp mrt0, v4, v5, v6, v7 done vm
	s_endpgm
