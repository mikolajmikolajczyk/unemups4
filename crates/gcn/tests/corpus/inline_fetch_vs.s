; inline_fetch_vs — GFX7 / Sea Islands (bonaire) vertex shader that CALLS a
; separate fetch shader for its attributes (task-113.4.2 AC #7).
;
; This is the caller half of the retail fetch-shader convention RE'd from the 5
; real Celeste VS (doc-6 Entry 9). Every one opens with the universal Orbis
; prologue then immediately calls its fetch shader:
;
;   s_mov_b32 vcc_hi, <imm>        ; the universal Orbis prologue (stashes a const)
;   s_swappc_b64 s[0:1], s[0:1]    ; CALL the fetch shader (its ptr is in s[0:1]);
;                                  ; saves the return PC back into s[0:1]
;   <main body reads the fetched v[4:7], exports>
;
; The fetch shader (fetch_pos_vs.s) loads the vertex-buffer V# and fetches the
; per-vertex position into v[4:7], then returns via s_setpc_b64 s[0:1]. After the
; call resolves (crate::resolve_fetch_call inlines the fetch body at the s_swappc),
; v[4:7] holds the fetched position, which this VS exports verbatim as pos0 and
; param0 — a pass-through, so the analytic expectation is the input position.
; Self-authored corpus; ZERO copyrighted assets.
;
; ABI:
;   s[0:1] : fetch-shader pointer (user data; the s_swappc address + return save)
;   s[2:3] : pointer to the V# descriptor set (consumed by the fetch shader)
;   v0     : vertex index (idxen fetch, in the fetch shader)
;   v[4:7] : fetched position (produced by the fetch shader, consumed here)
;
; NOTE: this .s does NOT itself assemble to a runnable standalone shader — its
; s_swappc_b64 must be resolved against fetch_pos_vs first. The differential
; harness resolves the pair before running/recompiling (see the fetch-call spec).
;
; Regenerate the raw bytes with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding inline_fetch_vs.s
; (see crates/gcn/tests/corpus/regen.sh).

	s_mov_b32 vcc_hi, 0x8
	s_swappc_b64 s[0:1], s[0:1]
	exp pos0, v4, v5, v6, v7 done
	exp param0, v4, v5, v6, v7
	s_endpgm
