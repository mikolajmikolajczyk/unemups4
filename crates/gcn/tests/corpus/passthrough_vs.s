; passthrough_vs — GFX7 / Sea Islands (bonaire) pass-through vertex shader.
;
; Fetches a vec4 position from the vertex-buffer V# in s[0:3] and exports it as
; both the clip-space position (pos0) and a param (param0) for the PS to read.
; Self-authored corpus shader (task-37); ZERO copyrighted assets.
;
; ABI (matches the semantic tables the corpus builder stamps):
;   s[2:3] : pointer to the fetch-shader / V# descriptor set (SGPR user data)
;   v0     : vertex index (idxen fetch)
;   s[0:3] : loaded position V# (128-bit buffer resource)
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding passthrough_vs.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	s_load_dwordx4 s[0:3], s[2:3], 0x0
	s_waitcnt lgkmcnt(0)
	buffer_load_format_xyzw v[4:7], v0, s[0:3], 0 idxen
	s_waitcnt vmcnt(0)
	exp pos0, v4, v5, v6, v7 done
	exp param0, v4, v5, v6, v7
	s_endpgm
