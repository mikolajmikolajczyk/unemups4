---
id: TASK-169
title: >-
  clock: cap BOOT-phase per-read delta to one 60Hz frame (Celeste warm-up
  matches real)
status: Done
assignee: []
created_date: '2026-07-18 05:45'
updated_date: '2026-07-18 11:26'
labels:
  - clock
  - celeste
  - retail
  - fidelity
dependencies: []
ordinal: 173000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-157 Phase-2 H2 experiment found the virtual clock's BOOT phase (pre-first-flip, core/src/clock.rs now_ns returning raw real host time) fast-forwards Celeste's intro past real hardware's phase: our frame0 emits 3 texture binds / 12 draws where real emits 2 / 10. A boot-phase per-read DELTA CAP — BOOT computed = real_ns().min(last + FRAME_NS) so each Update() delta is <=16.67ms while a rapidly-polling init spin-wait still catches up at +16.67ms/read and terminates fast — makes our frame0 become EXACTLY real's 2 binds / 10 draws and holds the 10-draw scene through frame4 (matching real's stable 10). Verified live (126 flips, boot flowed normally; a naive freeze-to-1us/read instead STALLED boot). This is a genuine fidelity improvement, independent of the task-157 texture-bind collapse (which it does NOT fix — left out of task-157 per its confirm-then-fix mandate). Also anchor the RENDER phase to the virtual boot time (LAST_NS) not real_ns() so there is no wall-time jump at the boot->render transition. Low-risk, localized to clock.rs. See task-157 PHASE 2 notes for the before/after bind-count table.
<!-- SECTION:DESCRIPTION:END -->
