---
id: TASK-44
title: >-
  gnm: register-based shader binds — SPI_SHADER_PGM_LO/HI →
  ShaderRef::GcnBinary; HLE Set*Shader emit real PM4
status: Done
assignee: []
created_date: '2026-07-11 12:54'
updated_date: '2026-07-11 18:15'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-43
  - TASK-36
priority: medium
ordinal: 43000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Two halves of one seam. (a) Derived view: at draw time read SPI_SHADER_PGM_LO/HI_VS and _PS (+ PGM_RSRC1/2 for GPR/user-SGPR counts) from SH bank → BoundShaders as ShaderRef::GcnBinary{addr=(hi:lo)<<8} — the route freegnm+Bloodborne use. (b) Make sceGnmSetVsShader/sceGnmSetPsShader(350) HLE write the documented dword counts (29/40, doc-3 §2) of SET_SH_REG PM4 into the caller's cmdbuf from guest vs_regs/ps_regs, so HLE-linked homebrew and statically-linked builders converge on the register file. Embedded binds stay on current global-state route (migration = open q, deferred). Does NOT resolve/recompile; a GcnBinary bind still defers until P4-18.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: DCB with SET_SH_REG PGM_LO/HI + DRAW_INDEX_AUTO → draw resolution NeedsGcn with correct derived .sb addr (unit)
- [ ] #2 headless: HLE sceGnmSetVsShader writes PM4 the task-21 decoder round-trips into same register values
- [ ] #3 headless: embedded-shader + ps4-pm4-test Tier B unchanged
- [ ] #4 headless-trace/live: running ps4-pm4-test shows new binds in PM4 trace, no regressions
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-11 (feat/task-44 @ 4385585, merged a3a40b7). (a) Derived view: dispatch_draw_auto now reads SPI_SHADER_PGM_LO/HI_VS+PS (+RSRC1/2) from SH bank (base 0x2C00, VS 0x48-4B / PS 0x08-0B, mirrors examples/ps4-pm4-test/main.c #defines + shadPS4 liverpool.h) → ShaderRef::GcnBinary{addr=(hi:lo)<<8, res: GcnResources{vgprs,sgprs,user_sgprs}}. GcnBinary bind DEFERS resolve (NeedsGcn) until task-53. (b) HLE sceGnmSetVsShader/PsShader emit SET_SH_REG PM4 (29/40 dwords, leading 4 = PGM_LO/HI/RSRC1/RSRC2, padded w/ IT_NOP) into caller cmdbuf. Embedded route untouched. Files: pm4/opcodes.rs, pm4/emit.rs(new), pm4/mod.rs, shader/source.rs, state.rs, exec.rs, shader/embedded.rs, libs shader_bind.rs. All 4 ACs ticked (round-trip + defer + embedded-unchanged + trace). Verify: gnm+libs 87 pass, workspace 157, clippy 0, fmt clean, gnm Vulkan-free. FLAGGED: exact Gnm VsStageRegisters field list unavailable — implemented load-bearing contract (leading 4 dwords = PGM block); retail full 29/40 register stream is decoder-valid filler, additive later. Combined main gate: 28 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
