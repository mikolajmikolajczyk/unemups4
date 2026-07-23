; wqm_bracket_ps — GFX7 / Sea Islands (bonaire) EXEC-save / whole-quad-mode bracket.
;
; The prologue retail pixel shaders wrap texture work in: save EXEC, switch to
; whole-quad mode (so helper lanes are live for derivatives), then restore EXEC.
; Here it brackets a plain constant-color export to prove the bracket is transparent
; to the exported result — the saved EXEC flows only back to EXEC on restore and
; never reaches a channel. Self-authored corpus shader (task-113.4.2 AC#4); ZERO
; copyrighted assets.
;
; In this HLE every invocation's quad is treated as fully covered, so s_wqm is the
; identity and the whole bracket is a no-op for the exported color: the oracle moves
; the real EXEC bits (restore reproduces the saved value) and the recompiler discards
; the bracket, and the two therefore agree on the export.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding wqm_bracket_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mov_b32 v0, 1.0
	v_mov_b32 v1, 0.25
	v_mov_b32 v2, 0.5
	v_mov_b32 v3, 1.0
	s_mov_b64 s[0:1], exec
	s_wqm_b64 exec, exec
	s_mov_b64 exec, s[0:1]
	exp mrt0, v0, v1, v2, v3 done vm
	s_endpgm
