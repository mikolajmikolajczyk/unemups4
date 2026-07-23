; inline_multi_fetch_vs — GFX7 / Sea Islands (bonaire) vertex shader that CALLS a
; TWO-STREAM fetch shader (task-153).
;
; The caller half of a multi-attribute fetch: it s_swappc's into fetch_vs.s, which
; recovers TWO distinct vertex-buffer V# from the descriptor set in s[2:3] — attr0
; (a vec4) at desc-set offset 0 into s[8:11]→v[4:7], and attr1 (a vec2) at offset 16
; into s[12:15]→v[8:9]. After the call resolves (resolve_fetch_call inlines the fetch
; body at the s_swappc), this VS exports attr0 as pos0 and attr1 (padded with z=0,
; w=1) as param0. The two attributes come from DIFFERENT V# with different bases and
; num_records, so a recompiler that collapsed both fetches onto one SSBO binding (the
; pre-task-153 bug) would read attr1 from attr0's buffer and export the WRONG param0.
; An exact interp==recompile match is the witness that each stream reaches its own V#.
; Self-authored corpus; ZERO copyrighted assets.
;
; ABI:
;   s[0:1]  : fetch-shader pointer (s_swappc address + return save)
;   s[2:3]  : pointer to the V# descriptor set (two V#: attr0 @0, attr1 @16)
;   v0      : vertex index (idxen fetch, in the fetch shader)
;   v[4:7]  : attr0 (fetched by the callee) — exported as pos0
;   v[8:9]  : attr1 (fetched by the callee) — exported (with 0,1) as param0
;
; NOTE: this .s does NOT assemble to a runnable standalone shader — its s_swappc_b64
; must be resolved against fetch_vs first (the differential harness does this).
;
; Regenerate the raw bytes with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding inline_multi_fetch_vs.s

	v_mov_b32 v10, 0
	v_mov_b32 v11, 1.0
	s_swappc_b64 s[0:1], s[0:1]
	exp pos0, v4, v5, v6, v7 done
	exp param0, v8, v9, v10, v11
	s_endpgm
