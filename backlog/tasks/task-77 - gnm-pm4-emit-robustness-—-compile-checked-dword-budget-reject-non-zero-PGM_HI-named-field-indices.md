---
id: TASK-77
title: >-
  gnm/pm4: emit robustness — compile-checked dword budget, reject non-zero
  PGM_HI, named field indices
status: Done
assignee: []
created_date: '2026-07-12 07:18'
updated_date: '2026-07-12 07:48'
labels:
  - gpu
  - gnm
dependencies: []
priority: low
ordinal: 76000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review findings #5 + #6 + #7 (emit.rs robustness, all in crates/gnm/src/pm4/emit.rs). #5: the 29/40-dword total is guarded ONLY by a debug_assert_eq (out.len()+NOP_DATA_BLOCK+1==total_dwords) which is elided in release. A future edit that adds/removes a register run without updating SET_VS/PS_SHADER_DWORDS silently emits a malformed stream in release (wrong-length NOP → the decoder swallows the next packet — exactly the task-69 bug, now invisible). FIX: derive total_dwords from the sum of the emitted runs + the NOP instead of a hardcoded magic constant (self-consistent, no assert needed), or use a const/compile-time assertion so a mismatch fails the build not silently. #6: PGM_HI is silently forced to 0; retail gnmdriver VALIDATES vs_regs[1]==0 / ps_regs[1]==0 and returns an ERROR on non-zero. Match retail: warn (tracing) and/or reject a non-zero PGM_HI rather than silently zeroing (which would otherwise produce a wrong pgm_addr → MagicNotFound with no signal). #7: the field->register mapping uses positional index literals (r(vs_regs, 4), r(vs_regs, 6), …) with no named constants — a VsStageRegisters/PsStageRegisters field addition silently mismaps and the debug_assert won't catch it (dword count unchanged). FIX: name the field indices (const VS_FIELD_SPI_VS_OUT_CONFIG: usize = 4; …) or a small mapping table so the index has a machine-checkable name. Keep the exact emitted packet layout unchanged (console-capture-verified); this is robustness-only, no behavior change. Vulkan-free; no task-NN in comments.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 total emitted dwords is self-derived or compile-time-checked (not only debug_assert): a run added/removed without updating the count fails the build or is impossible, not silent in release
- [ ] #2 a non-zero PGM_HI in the regs block is warned/rejected (matches retail's vs_regs[1]==0 validation), not silently zeroed
- [ ] #3 field->register indices are named constants / a table; the emitted VS(29)/PS(40) packet layout is byte-for-byte unchanged (existing round-trip tests pass)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-77 @<prior-history>, merged). #5: total_dwords now SELF-DERIVED = runs.map(run_dwords).sum()+1+NOP_DATA_BLOCK (run=header+offset+values); a run add/remove auto-adjusts, no wrong-length NOP in release. 29/40 const kept only as debug_assert_eq cross-check (guards doc-1 ABI drift, doesn't drive output). #6: shader_pgm_lohi helper returns [lo,0] (still forces 0=retail) but tracing::warn! on non-zero incoming HI (surfaces the anomaly retail rejects). #7: mod vs_field (7 consts) + mod ps_field (12 consts) replace all positional r(regs,N) literals — index tied by name to register. Emitted VS(29)/PS(40) bytes UNCHANGED (round-trip/derive/short-block/trailing-draw tests pass; + new derived_length_equals_documented_abi_totals + non_zero_pgm_hi_forced_to_zero). Verify: gnm+libs 102 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
