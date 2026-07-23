---
id: TASK-69
title: >-
  gnm/pm4: fix SET_SH_REG emitter — IT_NOP off-by-one (live bug), honor _size,
  dedup t3_header
status: Done
assignee: []
created_date: '2026-07-12 06:00'
updated_date: '2026-07-12 06:13'
labels:
  - gpu
  - gnm
dependencies: []
priority: high
ordinal: 68000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review findings #1/#6/#10 (all in pm4/emit.rs + libscegnmdriver/shader_bind.rs, one cohesive fix). #1 LIVE BUG: emit.rs:73 calls t3_header(op::IT_NOP, pad) but t3_header encodes count=body_len-1 and the NOP body is pad-1 dwords (the header is 1 of the pad slots) — the comment on line 72 even says '(pad-1) body dwords'. As written the NOP header claims 1 dword too many, so a decoder over-reads by one. The round-trip test passes only because it decodes the shader-set in ISOLATION (stream ends → NOP truncates harmlessly). In a real DCB where the guest appends a draw after sceGnmSetVsShader, the malformed NOP swallows the draw packet's header → the draw is silently dropped. Fix: t3_header(op::IT_NOP, pad - 1). #6: emit_into_cmdbuf (shader_bind.rs:28) + the emitters ignore the guest's reserved _size (sce_gnm_set_vs/ps_shader _size param) and always write 29/40 dwords via IdentityMem.write_bytes with no bound — a guest reserving fewer dwords gets adjacent guest memory overwritten. Honor _size (write min(reserved, 29/40) or reject/log when smaller). #7 folded in: read_reg_block (shader_bind.rs:43) returns [] on a null/unmapped regs ptr (read_array Err/empty → unwrap_or_default), which emits four zero PGM regs → the draw path derives GcnBinary{addr:0} (a shader bound at null) instead of no-bind; on a failed regs read, emit NOTHING (skip the shader-set) so no bogus bind is recorded. #10: t3_header is duplicated verbatim 4x (pm4/emit.rs:32, pm4/decode.rs:261, pm4/trace.rs:120, exec.rs:302) — promote one pub fn (pm4/opcodes.rs or a pm4 util) and call it from all four.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 IT_NOP padding uses pad-1; a DCB of [SET_VS_SHADER emit .. + DRAW_INDEX_AUTO] decodes BOTH the four SH regs AND the trailing draw (regression test with a draw after the shader-set)
- [ ] #2 emit honors _size: a reserved size < 29/40 does not write past the reservation (bounded write or logged skip), unit-tested
- [ ] #3 a null/unmapped vs_regs/ps_regs pointer results in NO bind (no GcnBinary{addr:0}), unit-tested
- [ ] #4 t3_header exists once (pub), the other 3 copies call it; build/clippy/fmt green
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-69 @ 4b56f8f, merged ba2efb5). (1) IT_NOP off-by-one FIXED: emit.rs now t3_header(IT_NOP, pad-1). Regression test trailing_draw_survives_after_shader_set builds [set_vs_shader..., DRAW_INDEX_AUTO] — confirmed FAILS on buggy code (draw dropped), passes fixed. (2) _size honored: emit_into_cmdbuf skips write + logs once when reserved != 0 && reserved < pm4.len() (safer than partial write); test undersized_reservation_skips_write_no_overflow (sentinel intact). (3) null regs → no bind: read_reg_block returns Option<Vec<u32>> (None on null/read-Err), handlers early-return, no GcnBinary{addr:0}; test null_regs_emits_no_pm4_no_bind. (4) t3_header deduped: one pub fn in pm4/opcodes.rs, 4 sites call it. Verify: gnm+libs 96 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined main gate: 28 suites ok, oracle 6/6. task-70 supersedes the NOP-pad path.
<!-- SECTION:NOTES:END -->
