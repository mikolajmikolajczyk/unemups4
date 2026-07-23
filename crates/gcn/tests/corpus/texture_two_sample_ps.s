; texture_two_sample_ps — GFX7 / Sea Islands (bonaire) TWO-texture pixel shader.
;
; A pixel shader that samples TWO DIFFERENT textures and exports the second
; result. This is the shape task-199 exists for: the GCN MIMG `srsrc`/`ssamp`
; operands are per-instruction, so one shader routinely mixes a T# that arrives
; MEMORY-resident (an `s_load_dwordx8` through a user-data descriptor-set
; pointer) with one that arrives REGISTER-resident (loaded straight into user
; SGPRs by the launch ABI). Before task-199 the recompiler collapsed every
; `image_sample` onto the first sample's descriptor, so the second sample
; silently read the first texture.
;
; The dependent chain is deliberate: sample A's result feeds sample B's
; coordinates, so a collapse is not merely a wrong binding — it changes the
; exported colour, which is exactly how it presented as a rendering bug.
;
; ABI:
;   s14       : interpolation parameter base (loaded into m0 before v_interp_*)
;   s[12:13]  : descriptor-set POINTER pair — the memory-resident T#/S# live in
;               the set it points at (T# at byte 0, S# at byte 0x20)
;   s[16:23]  : T# A (256-bit), fetched from the set  -> DescriptorSource::SetPointer
;   s[24:27]  : S# A (128-bit), fetched from the set
;   s[0:7]    : T# B (256-bit), inline in user SGPRs  -> DescriptorSource::InlineVSharp
;   s[8:11]   : S# B (128-bit), inline in user SGPRs
;   v0        : barycentric I, v1 : barycentric J (SPI-provided PS inputs)
;
; Self-authored corpus shader; ZERO copyrighted assets, no game/SDK bytes.
; Regenerate the OrbShdr blob with regen.sh (llvm-mc) then the corpus.rs builder.

	s_mov_b32 m0, s14
	s_load_dwordx8 s[16:23], s[12:13], 0x0
	s_load_dwordx4 s[24:27], s[12:13], 0x8
	v_interp_p1_f32 v2, v0, attr0.x
	v_interp_p2_f32 v2, v1, attr0.x
	v_interp_p1_f32 v3, v0, attr0.y
	v_interp_p2_f32 v3, v1, attr0.y
	s_waitcnt lgkmcnt(0)
	image_sample v[4:7], v[2:3], s[16:23], s[24:27] dmask:0xf
	s_waitcnt vmcnt(0)
	image_sample v[8:11], v[4:5], s[0:7], s[8:11] dmask:0xf
	s_waitcnt vmcnt(0)
	exp mrt0, v8, v9, v10, v11 done vm
	s_endpgm
