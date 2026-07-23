; cbuffer_ps — GFX7 / Sea Islands (bonaire) scalar constant-buffer pixel shader.
;
; Loads an RGBA constant from a uniform (constant) buffer with s_buffer_load — the
; SBASE (s[0:3]) names a 128-bit V# descriptor, and the load reads dwords from
; V#.base + offset into s[4:7]. Then it moves them to v0..v3 and exports. This is the
; scalar-constant path retail pixel shaders use for material/uniform values (distinct
; from the vertex-buffer V# read via MUBUF). Self-authored corpus shader
; (task-113.4.2 AC#5); ZERO copyrighted assets.
;
; The descriptor lives in user SGPRs s0..s3 (the driver/provider places it); the
; oracle reads V#.base from it and loads the real bytes through the VMM.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding cbuffer_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	s_buffer_load_dwordx4 s[4:7], s[0:3], 0x0
	s_waitcnt lgkmcnt(0)
	v_mov_b32 v0, s4
	v_mov_b32 v1, s5
	v_mov_b32 v2, s6
	v_mov_b32 v3, s7
	exp mrt0, v0, v1, v2, v3 done vm
	s_endpgm
