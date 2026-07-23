; interp_color_ps — GFX7 / Sea Islands (bonaire) interpolating-color pixel shader.
;
; Barycentrically interpolates a per-vertex RGB varying (attr0.xyz) using the
; two-phase GCN interpolation (v_interp_p1_f32 then v_interp_p2_f32 per channel),
; then exports it with alpha = 1.0 to render target 0 (mrt0). This is the classic
; Gouraud-shaded-triangle PS. Self-authored corpus shader (task-37); ZERO
; copyrighted assets.
;
; ABI:
;   s0     : interpolation parameter base (loaded into m0 before v_interp_*)
;   v0     : barycentric I, v1 : barycentric J (SPI-provided PS inputs)
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding interp_color_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	s_mov_b32 m0, s0
	v_interp_p1_f32 v2, v0, attr0.x
	v_interp_p2_f32 v2, v1, attr0.x
	v_interp_p1_f32 v3, v0, attr0.y
	v_interp_p2_f32 v3, v1, attr0.y
	v_interp_p1_f32 v4, v0, attr0.z
	v_interp_p2_f32 v4, v1, attr0.z
	v_mov_b32 v5, 1.0
	exp mrt0, v2, v3, v4, v5 done vm
	s_endpgm
