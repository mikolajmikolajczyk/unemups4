; nonstd_stride_vs — GFX7 / Sea Islands (bonaire) pass-through vertex shader that
; fetches a NON-16-byte-stride vertex attribute (task-128 / task-130 slice 5).
;
; Identical in shape to passthrough_vs, but fetches only xyz (a 12-byte element) so
; the per-vertex stride can be exercised at 12 / 24 / 32 bytes without the vec4 fetch
; spilling into the next vertex. The clip-space w is set to 1.0 (a constant) before
; the fetch. The recompiler reads the vertex stride from a SPIR-V PUSH CONSTANT, so ONE
; recompiled module serves every stride — the provider pushes the guest V#'s real stride
; per draw (task-140). Self-authored corpus shader; ZERO copyrighted assets.
;
; ABI (matches passthrough_vs):
;   s[2:3] : pointer to the V# descriptor set (SGPR user data)
;   v0     : vertex index (idxen fetch)
;   s[0:3] : loaded position V# (128-bit buffer resource; word1[29:16] = stride)
;
; Attribute table this parses to:
;   attr 0: V# in s0, dest v4, 3 components (xyz), desc-set s[2:3] offset 0
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding nonstd_stride_vs.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v7, 1.0
	s_load_dwordx4 s[0:3], s[2:3], 0x0
	s_waitcnt lgkmcnt(0)
	buffer_load_format_xyz v[4:6], v0, s[0:3], 0 idxen
	s_waitcnt vmcnt(0)
	exp pos0, v4, v5, v6, v7 done
	exp param0, v4, v5, v6, v7
	s_endpgm
