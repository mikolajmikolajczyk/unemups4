---
id: TASK-210
title: >-
  libs/gnm: per-draw and per-bind logging sits at info! — the default log level
  emits 1300 lines/s during gameplay
status: To Do
assignee: []
created_date: '2026-07-21 19:19'
labels:
  - gnm
  - libs
  - dx
  - perf
dependencies: []
priority: medium
ordinal: 215000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A maintainer gameplay run at the default log level produced 109801 lines in 83.7 s (1311 lines/s, 14.6 MB), formatted synchronously on the guest thread. The traffic is per-draw and per-bind:

    15914 x [GNM] sceGnmDrawIndexOffset offset=.. count=..
    11116 x [GNM] sceGnmDrawIndexAuto count=..
     8888 x Kernel: tls_set_specific tid=.. key=.. val=..
     6432 x [GNM] sceGnmSetPsShader.. regs=..
     6261 x [GNM] sceGnmSetVsShader regs=..
     2603 x [GNM]   [0] dcb=.. (.. B) ccb=.. (.. B)

These are per-draw diagnostics, not events a default run should report. info! is the level a user sees without asking for anything; anything emitted once per draw or once per bind belongs at debug!.

The cost is unquantified and that is part of the work: it confounded a perf measurement in the session that found it (the run had to be repeated with RUST_LOG=error before the numbers meant anything). Measure it rather than assuming it is negligible OR that it is large.

Sites are in crates/libs/src/libscegnmdriver/ (draw.rs, shader_bind.rs, submit.rs) and the kernel bridge tls_set_specific. Demote per-draw / per-bind / per-submit logging to debug!, leaving genuinely once-per-run or error-path messages at info!/warn!. Do not silence anything a failure path depends on — the retail bring-up method leans on these logs, so the information must remain reachable at debug!, not deleted.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 per-draw, per-bind and per-submit GNM logging plus tls_set_specific are at debug!, with once-per-run and error-path messages left at their current level
- [ ] #2 measured: lines/s and MB at the default level during gameplay, before and after; and the frame-time difference between the default level and RUST_LOG=error, reported as a number
- [ ] #3 the demoted messages are still reachable at debug! — the doc-4 smoke-loop diagnostics lose nothing
- [ ] #4 build + clippy clean, cargo test --workspace green
<!-- AC:END -->
