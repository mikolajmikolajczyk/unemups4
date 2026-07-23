; offset_fetch_vs — GFX7 / Sea Islands (bonaire) pass-through vertex shader whose
; vertex fetch carries a NON-ZERO MUBUF immediate offset (task-153 Bug 1).
;
; Identical in shape to passthrough_vs, but the buffer_load_format has `offset:8`, so
; the fetched byte address is base + index*stride + 8 (the oracle adds the immediate
; offset; the recompiler must too, or it silently reads element 0's dword and produces
; the wrong attribute). A recompiler that DROPPED the offset (the pre-task-153 bug)
; would fetch from base + index*stride, so an exact interp==recompile match witnesses
; that the immediate offset threads into the recompiled fetch address.
; Self-authored corpus; ZERO copyrighted assets.
;
; ABI (matches passthrough_vs):
;   s[2:3] : pointer to the V# descriptor set (SGPR user data)
;   v0     : vertex index (idxen fetch)
;   s[0:3] : loaded position V# (128-bit buffer resource)
;
; Attribute table this parses to:
;   attr 0: V# in s0, dest v4, 4 components (xyzw), desc-set s[2:3] offset 0, MUBUF offset 8
;
; Regenerate the raw bytes with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding offset_fetch_vs.s

	s_load_dwordx4 s[0:3], s[2:3], 0x0
	s_waitcnt lgkmcnt(0)
	buffer_load_format_xyzw v[4:7], v0, s[0:3], 0 idxen offset:8
	s_waitcnt vmcnt(0)
	exp pos0, v4, v5, v6, v7 done
	exp param0, v4, v5, v6, v7
	s_endpgm
