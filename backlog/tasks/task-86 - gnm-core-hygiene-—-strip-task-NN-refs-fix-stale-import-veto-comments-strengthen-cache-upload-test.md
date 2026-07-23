---
id: TASK-86
title: >-
  gnm/core: hygiene — strip task-NN refs, fix stale import-veto comments,
  strengthen cache upload test
status: Done
assignee: []
created_date: '2026-07-12 10:21'
updated_date: '2026-07-12 10:39'
labels:
  - gpu
  - gnm
  - core
  - chore
dependencies: []
priority: low
ordinal: 85000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review round-5 cross-crate hygiene (non-gcn; task-83/84 leftovers). (#8-conventions) exec.rs:925 comment references 'task-42' — strip per conventions.md:20 ('Don't reference the current task/fix/PR ... belongs in the commit message'); reword to state the invariant (resolve-once guards a future provider from double parse+recompile). (#C4) task-83 made replay_import PANIC (assert!) on import decline, but inline comments in crates/gnm/src/exec.rs and the PresentSink trait doc in crates/core/src/gpu.rs (and any other) still say 'logged hard error' — update them to 'panics — fatal invariant violation' so the severity is consistent everywhere; the cache/mod.rs + ImportProbe docs already say it. (#C5) the new cache test probe_declines_emits_copy_path_no_import (cache/tests.rs) uses zero-filled data (vec![0u8;0x100]) so it cannot distinguish a correct-content upload from a zero-initialized buffer — give the test-mem a NON-ZERO byte pattern and assert the UploadBuffer carries those exact bytes. (#7 NOTE-only, do NOT change behavior) the import-veto assert-on-display-thread depth (a runtime per-pointer decline the probe can't fully predict at boot → panic crashes the emulator) was questioned by review. The maintainer's task-83 decision was deliberate fail-fast + it is unreachable with the default copy-side policy. Do NOT change the assert; instead record a one-line revisit note (in the replay_import doc or deferred.md) that when a non-copy-side import policy is enabled (task-53/55), the assert-vs-graceful-degrade tradeoff must be reconsidered.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 no task-NN reference remains in exec.rs source comments (git grep clean); the invariant is stated without the ticket number
- [ ] #2 all inline comments describing the import-decline path say 'panics' consistently (no stale 'logged hard error')
- [ ] #3 the cache decline test uses non-zero content and asserts the exact uploaded bytes
- [ ] #4 a revisit note for the import-veto assert-vs-graceful tradeoff is recorded (behavior unchanged); build/test/clippy/fmt green
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-86 @<prior-history>, merged). #8: exec.rs:925 task-42 ref reworded to invariant (resolve each stage once so side-effecting provider parse+recompile not run twice/draw); git grep task-NN in exec.rs clean. #C4: stale 'logged hard error' → 'panics — fatal invariant violation, never silent copy fallback' in core/gpu.rs:158 (only occurrence; none in exec.rs/cache/mod.rs). #C5: probe_declines_emits_copy_path_no_import now pre-fills mem non-zero ramps (0x1000..: i+0xA0, 0x2000..: i+0xB0) + asserts UploadBuffer carries exact bytes (both cases). #7: deferred.md new entry 'Import-veto assert-vs-graceful-degrade' — revisit when non-copy-side import policy enabled (task-53/55); behavior unchanged. 105 tests, clippy 0, fmt clean, grep clean. Combined gate: 30 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
