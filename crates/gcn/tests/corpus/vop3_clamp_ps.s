; vop3_clamp_ps — GFX7 / Sea Islands (bonaire) VOP3 `clamp` output modifier (task-188).
;
; Exercises the VOP3 clamp bit (low-dword bit 11) — what `saturate()` compiles to —
; both ALONE and COMBINED with `omod`, because the two are applied in a fixed order:
; the hardware chain is raw result -> omod -> clamp. Retail Celeste reaches this: the
; dumped pixel shader ps-27883c9d7c88cd30 carries `v_mac_f32 ... clamp` on a value it
; then uses as a lerp factor, so an unclamped result is visible in the output.
;
; Channel by channel (every value exact in f32, so the analytic expectation is exact):
;
;   r: 2.0*2.0 + 1.0  =  5.0   -> clamp        -> 1.0    (saturates from above)
;   g: 2.0*-2.0 + 1.0 = -3.0   -> clamp        -> 0.0    (saturates from below)
;   b: 0.5*0.5 + 0.5  =  0.75  -> mul:2, clamp -> 1.0    (clamp-then-mul would give 1.5)
;   a: 1.0*1.0 + 0.5  =  1.5   -> div:2, clamp -> 0.75   (clamp-then-div would give 0.5)
;
; b and a are the ORDER probes: each has a different value under the reversed chain, so
; applying clamp before omod fails this shader rather than passing it silently.
; Self-authored corpus shader (task-188); ZERO copyrighted assets.
;
; Regenerate the OrbShdr blob with:
;   llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding vop3_clamp_ps.s
; then wrap via crates/gcn/tests/corpus.rs (see that module's regen note).

	v_mad_f32 v2, 2.0, 2.0, 1.0 clamp
	v_mad_f32 v3, 2.0, -2.0, 1.0 clamp
	v_mad_f32 v4, 0.5, 0.5, 0.5 clamp mul:2
	v_mad_f32 v5, 1.0, 1.0, 0.5 clamp div:2
	exp mrt0, v2, v3, v4, v5 done vm
	s_endpgm
