---
id: TASK-70
title: >-
  gnm/pm4: emit the real VS/PS register run in Set*Shader (drop NOP-pad bandaid)
  — unblocks real-game binds
status: Done
assignee: []
created_date: '2026-07-12 06:00'
updated_date: '2026-07-12 06:51'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-69
priority: medium
ordinal: 69000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding #4 (altitude). emit.rs currently writes only the 4 leading PGM registers [PGM_LO,HI,RSRC1,RSRC2] then pads the remaining 25/36 dwords with a decoder-valid IT_NOP. The retail 29/40-dword VsStageRegisters/PsStageRegisters stream carries meaningful state (SPI_SHADER_PGM_RSRC3, SPI_VS_OUT_CONFIG, SPI_SHADER_POS_FORMAT, PA_CL_VS_OUT_CNTL, SPI_PS_INPUT_*, SPI_SHADER_COL_FORMAT, user_data SGPRs, etc.). Real HLE-linked games get zeros for all of it → wrong vertex-output config / pixel-input config at draw time. This HARD-BLOCKS task-53 (real-shader draw) for anything but the synthetic corpus (which sidesteps it by emitting user_data via a separate packet). FIX: obtain the exact VsStageRegisters/PsStageRegisters field→register-offset layout (OpenOrbis GnmDriver.h / shadPS4 liverpool), have read_reg_block read the FULL block and emit_shader_set write the real SET_SH_REG/SET_CONTEXT_REG runs the struct maps to, removing the NOP-pad entirely. SUPERSEDES task-69's NOP-off-by-one fix (the NOP path goes away). Sequence: do this alongside / just before task-53; it shares files with task-69 so land 69 first, then this rewrites the padding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 read_reg_block reads the full 29/40-dword register block; emit writes the real SET_SH_REG/SET_CONTEXT_REG runs (no IT_NOP filler)
- [ ] #2 the decoder round-trips every meaningful register (RSRC3, SPI_VS_OUT_CONFIG, POS_FORMAT, user_data, PS input/col-format) into the correct bank, unit-tested against a known VsStageRegisters/PsStageRegisters layout
- [ ] #3 field→offset layout sourced + cited (OpenOrbis/shadPS4); embedded + ps4-pm4-test Tier B unchanged
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-70 @ 24aea2c, merged). Set*Shader now emits REAL register runs from the full VsStageRegisters/PsStageRegisters block (no fake 4-regs+big-NOP). Layout sourced+cited = shadPS4 src/core/libraries/gnmdriver/gnmdriver.cpp + Mesa sid.h GFX6 offsets; INDEPENDENTLY CONFIRMED by a research subagent (exact 29/40 dword breakdown). VS(29): SetShReg PGM_LO/HI(=0)@0x48, SetShReg RSRC1/2@0x4A, SetContextReg PA_CL_VS_OUT_CNTL@0x207, SPI_VS_OUT_CONFIG@0x1B1, SPI_SHADER_POS_FORMAT@0x1C3, then IT_NOP<11>=12. PS(40): SetShReg PGM_LO/HI@0x08, RSRC1/2@0x0A, SetContextReg Z/COL_FORMAT@0x1C4, PS_INPUT_ENA/ADDR@0x1B3, PS_IN_CONTROL@0x1B6, BARYC_CNTL@0x1B8, DB_SHADER_CONTROL@0x203, CB_SHADER_MASK@0x8F, IT_NOP<11>. KEY: the trailing IT_NOP<11> is AUTHENTIC retail (shadPS4 emits WriteTrailingNop<11> so sceGnmUpdate*Shader can overwrite in-place) — 'drop NOP-pad' = drop the FAKE 4-regs+zero-state, keep the real register runs + real small NOP. NOT emitted by retail (nor us): RSRC3, user_data/SGPR loads, context preamble. PGM_HI forced 0. FLAG task-53: VS RSRC1 normally mixes shader_modifier (left to caller); apply in HLE if 53 needs it. Verify: gnm+libs 100 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined main gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
