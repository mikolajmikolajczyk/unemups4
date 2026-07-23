; m0_save_ps — GFX7 / Sea Islands (bonaire) m0 save/restore idiom.
;
; Exercises m0 as a plain scalar register: retail vertex shaders SAVE the launch m0
; (`s_mov s, m0`), clobber it locally, and restore it (e.g. Celeste's sh03). m0 is
; NOT consulted for interpolation (attribute selection comes from the VINTRP field),
; so both oracle and recompiler treat m0 as an ordinary u32 slot with a launch default
; of 0. This shader validates two paths:
;   - read m0 BEFORE any in-shader write → the default 0 (s5 → v0 = 0.0);
;   - write m0 then read it back → a faithful copy (m0 = bits of 50.0 → s6 → v1 = 50.0).
; Exports (0.0, 50.0, 1.0, 1.0), exact in f32. Self-authored corpus shader
; (task-113.4.2); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding m0_save_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	s_mov_b32 s5, m0
	s_mov_b32 m0, 0x42480000
	s_mov_b32 s6, m0
	v_mov_b32 v0, s5
	v_mov_b32 v1, s6
	v_mov_b32 v2, 1.0
	exp mrt0, v0, v1, v2, v2 done vm
	s_endpgm
