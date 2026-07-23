---
id: TASK-66
title: 'gnm/core: debug tripwire for display-thread-never-locks-driver() invariant'
status: To Do
assignee: []
created_date: '2026-07-11 18:17'
labels:
  - gpu
  - gnm
  - core
dependencies: []
priority: medium
ordinal: 65000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable Runda-1 review SHOULD-FIX #3. The task-43 invariant (display thread must NEVER acquire driver() — instant silent hang: guest holds driver() blocked on display channel while display blocks on driver()) is comment-enforced only (crates/gnm/src/driver.rs:141-156). No deadlock cycle exists today (all 13 driver() call sites are guest-thread HLE in crates/libs; crates/gpu has no driver()), but task-49 puts resource-cache state into driver-owned GpuState and BackendCmds start referencing resources — the display thread reaching the cache via driver() becomes an easy accident. FIX: record the display thread's ThreadId at run_display_loop entry (a OnceLock in ps4-core), and debug_assert! in driver() that the current thread isn't it. One-line insurance converting a hang into a caught panic at the offending call site.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 display thread ThreadId recorded once at run_display_loop entry (OnceLock in ps4-core)
- [ ] #2 driver() debug_asserts current thread != display thread; a deliberate test that locks driver() from the display thread panics in debug
- [ ] #3 no release-build cost (debug_assert only); existing tests + oracle 6/6 unaffected
<!-- AC:END -->
