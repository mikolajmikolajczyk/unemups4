; fetch_vs — GFX7 / Sea Islands (bonaire) vertex fetch-shader subroutine.
;
; A fetch shader is the small GCN subroutine the driver points a VS at through
; user SGPRs: it loads one buffer resource (V#) per vertex attribute from the
; descriptor set in s[2:3], `buffer_load_format`s each attribute (idxen, driven
; by the vertex index in v0) into an agreed destination VGPR block, then returns
; to the main VS with `s_setpc_b64` — it does NOT `s_endpgm` (it is a callee, not
; a program). Self-authored corpus shader; ZERO copyrighted assets.
;
; ABI (the real gnmx fetch-shader convention; GCN semantics per the AMD Sea Islands ISA):
;   s[2:3]  : pointer to the V# descriptor set (user data)
;   v0      : vertex index (idxen fetch)
;   s[8:11] : loaded attribute-0 V# (128-bit buffer resource)
;   s[12:15]: loaded attribute-1 V#
;   v[4:7]  : attribute 0 destination (vec4)
;   v[8:9]  : attribute 1 destination (vec2)
;   s[0:1]  : return address (s_setpc_b64 target)
;
; Attribute table this parses to (hand-reasoned from the semantics above):
;   attr 0: V# in s8,  dest v4, 4 components, desc-set s[2:3] offset 0
;   attr 1: V# in s12, dest v8, 2 components, desc-set s[2:3] offset 16
;
; Regenerate the raw bytes with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding fetch_vs.s
; (see crates/gcn/tests/corpus/regen.sh).

	s_load_dwordx4 s[8:11], s[2:3], 0x0
	s_load_dwordx4 s[12:15], s[2:3], 0x4
	s_waitcnt lgkmcnt(0)
	buffer_load_format_xyzw v[4:7], v0, s[8:11], 0 idxen
	buffer_load_format_xy v[8:9], v0, s[12:15], 0 idxen
	s_waitcnt vmcnt(0)
	s_setpc_b64 s[0:1]
