; fetch_pos_vs — GFX7 / Sea Islands (bonaire) vertex fetch-shader subroutine.
;
; A fetch shader is the small GCN subroutine the gnmx driver points a VS at
; through a user-SGPR pair: it loads the vertex-buffer V# from the descriptor set
; in s[2:3], `buffer_load_format`s the per-vertex attribute (idxen, driven by the
; vertex index in v0) into an agreed destination VGPR block, then returns to the
; main VS with `s_setpc_b64 s[0:1]` — it does NOT `s_endpgm` (it is a callee, not a
; program). The `s_swappc_b64` in the caller (see inline_fetch_vs.s) saved the
; return PC into s[0:1] before jumping here. Self-authored corpus; ZERO copyrighted
; assets.
;
; ABI (mirrors the real gnmx fetch-shader convention — the same one fetch_vs.s uses):
;   s[2:3]  : pointer to the V# descriptor set (user data the driver preloads)
;   v0      : vertex index (idxen fetch)
;   s[8:11] : loaded position V# (128-bit buffer resource)
;   v[4:7]  : attribute-0 (position) destination (vec4)
;   s[0:1]  : return address (s_setpc_b64 target — saved by the caller's s_swappc)
;
; A single attribute (the vec4 position) so the inline_fetch_vs caller is a clean
; pass-through: after this returns, v[4:7] holds the fetched position, which the
; caller exports verbatim as pos0 and param0. Attribute table this parses to:
;   attr 0: V# in s8, dest v4, 4 components, desc-set s[2:3] offset 0
;
; Regenerate the raw bytes with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding fetch_pos_vs.s
; (see crates/gcn/tests/corpus/regen.sh).

	s_load_dwordx4 s[8:11], s[2:3], 0x0
	s_waitcnt lgkmcnt(0)
	buffer_load_format_xyzw v[4:7], v0, s[8:11], 0 idxen
	s_waitcnt vmcnt(0)
	s_setpc_b64 s[0:1]
