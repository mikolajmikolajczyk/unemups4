---
id: TASK-35
title: 'gnm: extend ps4-pm4-test Tier B to drive embedded-shader draw end-to-end'
status: Done
assignee: []
created_date: '2026-07-11 11:48'
updated_date: '2026-07-11 13:42'
labels:
  - gnm
  - gpu
dependencies: []
priority: medium
ordinal: 34000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Corpus gap found during task-24 (phase-3.5 embedded draw). The Tier-B path of examples/ps4-pm4-test hand-emits SET_SH_REG PM4 but does NOT call sceGnmSetEmbeddedVsShader(...,0)/sceGnmSetEmbeddedPsShader(...,1) before sceGnmDrawIndexAuto, so it does not drive the new bound-shader -> IT_DRAW_INDEX_AUTO executor arm (task-24) end-to-end. Those NIDs are now stubbed (task-20/31), unlike when the ELF was written (doc-1 3.4: embedded VS id 0 = fullscreen quad, PS id 1 = R/G export; needs no .sb blob/compiler). Add a Tier-B (or new Tier-C) sequence to the test ELF Makefile: SetEmbeddedVs(0) + SetEmbeddedPs(1) + DrawIndexAuto, so a live run (LD_LIBRARY_PATH=/usr/lib) exercises the embedded host SPIR-V pipeline + present and confirms task-24 AC#2. Do NOT add this to the 6-example headless oracle baseline set (a live GPU draw needs a driver). Keep the existing Tier A/B intact; this is additive corpus.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Tier-B (or new tier) of examples/ps4-pm4-test calls sceGnmSetEmbeddedVsShader(id 0) + sceGnmSetEmbeddedPsShader(id 1) before sceGnmDrawIndexAuto, rebuilt .elf committed
- [x] #2 A live run with a Vulkan driver drives the task-24 embedded-draw arm end-to-end (BindEmbeddedPipeline + DrawAuto reach AshBackend), maintainer-verified (confirms task-24 AC#2)
- [x] #3 The 6-example headless oracle set is unchanged (new tier NOT added to run_examples.sh baselines); existing Tier A/B intact
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Toolchain installed at data/oo_sdk (gitignored, v0.5.4 toolchain-llvm-18, build verified). Edit run_tier_b in main.c: call sceGnmSetEmbeddedVsShader(cmd,29,0,0)+sceGnmSetEmbeddedPsShader(cmd,40,1) for their now-stubbed HLE side-effect (records bound embedded id → drives task-24 DrawIndexAuto arm); NOT advancing cmd (stub writes no PM4), keep hand-emitted SET_SH_REG for task-21 decoder. Update stale header comment #2. Rebuild .elf with OO_PS4_TOOLCHAIN, copy to committed path. Verify headless: [GNM] SetEmbeddedVs id=0/Ps id=1 logs appear; 6-example oracle untouched (ps4-pm4-test not in set). AC#2 live draw still needs maintainer GPU.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-11 (main-loop, inline — small edit + rebuild).

TOOLCHAIN: OpenOrbis PS4 toolchain installed at data/oo_sdk (gitignored; release v0.5.4 toolchain-llvm-18.tar.gz, 151MB extracted; provides link.x + lib/crt1.o + libc/libkernel/libSceGnmDriver stubs + include/). System clang22/lld22 build the freebsd12-elf target against that sysroot. This ALSO fixes the ps4-syscalls build.rs 'SDK path not found' warnings (build.rs reads data/oo_sdk/include). NOTE: data/oo_sdk is gitignored — NOT committed; must be reinstalled per-checkout (docs/dev-setup already references cloning it).

CHANGE: examples/ps4-pm4-test/ps4-pm4-test/main.c run_tier_b now calls sceGnmSetEmbeddedVsShader(cmd,29,0,0) + sceGnmSetEmbeddedPsShader(cmd,40,1) before the draw (exact SDK prototypes from data/oo_sdk/include/orbis/GnmDriver.h). These NIDs are HLE-stubbed (task-24) and RECORD the bound embedded shader id into ps4-gnm state, which the IT_DRAW_INDEX_AUTO executor arm resolves to the hardcoded host SPIR-V pipeline. cmd is NOT advanced (emulator stub writes no PM4); the equivalent SH-register PM4 stays hand-emitted for task-21's decoder. Header comment #2 updated (was stale: claimed the embedded builders are un-stubbed). Rebuilt .elf (make eboot.bin) copied to examples/ps4-pm4-test/ps4-pm4-test.elf (85.1K).

VERIFIED (headless): elf builds clean; llvm-objdump shows both calls in .text (callq @plt); llvm-readelf shows both as UND dyn-sym imports; both NIDs (+AFvOEXrKJk / X9Omw9dwv5M) are registered HLE handlers (loader resolves, no missing-symbol); 6-example oracle 6/6 (ps4-pm4-test not in the set; Tier A/B intact).

AC#2 = LIVE ONLY (maintainer, GPU). Headless CANNOT reach Tier B: task-34 wired present_sink unconditionally (GpuManager exists even headless), so Tier A's SubmitAndFlip blocks on the vsync handshake the dead headless display loop never signals (same class as softgpu's videoout flip). So the embedded-draw path (SetEmbedded logs + BindEmbeddedPipeline/DrawAuto dispatch + R/G present) is only observable with a real display+GPU: LD_LIBRARY_PATH=/usr/lib cargo run --release -p unemups4 -- examples/ps4-pm4-test/ps4-pm4-test.elf (watch for [GNM] sceGnmSetEmbeddedVsShader id=0 / PsShader id=1, then a drawn R/G frame on Tier B).

UNCOMMITTED — awaiting maintainer review/commit (main.c + ps4-pm4-test.elf) + live-verify of AC#2.
<!-- SECTION:NOTES:END -->

## Comments

<!-- COMMENTS:BEGIN -->
created: 2026-07-11 13:42
---
Live-verified 2026-07-11 (maintainer, GPU): the added sceGnmSetEmbeddedVsShader(0)/PsShader(1) calls fire in the [GNM] log and drive the task-24 DrawIndexAuto arm end-to-end → R/G frame rendered. AC#2 confirmed; task Done.
---
<!-- COMMENTS:END -->
