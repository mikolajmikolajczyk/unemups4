; flat_color_ps — GFX7 / Sea Islands (bonaire) flat-color pixel shader.
;
; Exports a constant RGBA (1.0, 0.25, 0.5, 1.0) to render target 0 (mrt0). No
; interpolation, no varyings — the simplest PS that produces a visible color.
; Self-authored corpus shader (task-37); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding flat_color_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 0x3e800000
	v_mov_b32 v2, 0.5
	v_mov_b32 v3, 1.0
	exp mrt0, v0, v1, v2, v3 done vm
	s_endpgm
