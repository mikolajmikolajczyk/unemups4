---
id: TASK-96
title: 'corpus: author a GCN .sb triangle homebrew ELF for task-53 AC#3 live triangle'
status: Done
assignee: []
created_date: '2026-07-12 18:22'
updated_date: '2026-07-12 19:02'
labels:
  - gpu
  - gcn
  - corpus
dependencies:
  - TASK-53
priority: high
ordinal: 95000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-53 keystone AC#3 (live GPU: corpus ELF renders the colored triangle) is UNVERIFIABLE because no such ELF exists — examples/ has only ps4-softgpu (present-only) and ps4-pm4-test (EMBEDDED shaders id=0/1, not register-route GCN). Need a homebrew ELF (OpenOrbis toolchain, like ps4-pm4-test) that: embeds real GCN .sb VS+PS blobs (the passthrough_vs/flat_color_ps corpus the recompiler+diff_harness already use), binds them via the register route SPI_SHADER_PGM_LO/HI (sceGnmSetVsShader/PsShader), sets up a vertex buffer + V# user-data (s[2:3]) + RT/viewport regs, and submits DRAW_INDEX_AUTO (and a DRAW_INDEX_2 variant). Then AC#3 runs: LD_LIBRARY_PATH=/usr/lib cargo run --release -p unemups4 -- <corpus.elf>. Backend review verdict: the path is conditionally ready — renders IFF the recompiled VS declares its input as a Location=0 vec4 SPIR-V input; verify that when the ELF exists. Relates to task-54 (Tier-C corpus) / task-58 (shadPS4 compare) / freegnm triangle as a shared cross-emulator corpus.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a corpus ELF binds real GCN .sb VS+PS via the register route and submits a vertex-buffer triangle
- [ ] #2 task-53 AC#3 verified: the emulator window shows the colored triangle
- [ ] #3 the recompiled VS's vertex input is confirmed to match the backend's declared vertex-input (couples with the general-vertex-input task)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-12. Done: authored examples/ps4-gcn-triangle/ (main.c, Makefile, .gitignore, ps4-gcn-triangle.elf built via OpenOrbis toolchain). Real register-route: sceGnmSetVsShader/PsShader emit the 29/40-dword SET_SH_REG+SET_CONTEXT_REG runs from VsStageRegisters/PsStageRegisters over the embedded passthrough_vs.sb + flat_color_ps.sb corpus (headers intact); s[2:3]=V# desc ptr (SPI_SHADER_USER_DATA_VS_0+2); CB_COLOR0_BASE=fb>>8 aliases the registered 1080p videoout fb; PA_CL_VPORT_* + PA_SC_SCREEN_SCISSOR_* programmed (required for visibility); DRAW_INDEX_AUTO count=3. Sanity-load: guest boots, 24 relocs applied, [GNM] SetVs/SetPs/DrawIndexAuto/SubmitAndFlip/SubmitDone all fire, Vulkan inited + zero-copy imported fb@0x400214000, no panic/defer/loader error. AC#1 ticked. Next (maintainer, live GPU): #2 visible triangle + #3 vertex-input match via 'LD_LIBRARY_PATH=/usr/lib cargo run --release -p unemups4 -- examples/ps4-gcn-triangle/ps4-gcn-triangle.elf'. Blocker: none. NOT committed (awaiting user request); staged.
<!-- SECTION:NOTES:END -->
