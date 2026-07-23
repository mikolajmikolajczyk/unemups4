---
id: TASK-209
title: >-
  gpu/cpu/diag: instrument the guest frame — 26ms of a 50ms frame lives outside
  the flip syscall with zero visibility
status: Done
assignee: []
created_date: '2026-07-21 19:19'
updated_date: '2026-07-21 22:01'
labels:
  - diag
  - perf
  - cpu
  - gpu
  - dx
dependencies:
  - TASK-203
priority: high
ordinal: 214000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-203 closed the flip syscall's budget to 0.0% unaccounted, and task-204 cut the flip from 38.4 ms to 23.9 ms. The frame rate did not move: Celeste gameplay stayed at 20 fps, 50 ms per frame. The time did not vanish, it RELOCATED — the share outside the flip went from 11.6 ms to 26.1 ms. The same work-moves-but-wall-time-does-not pattern appeared twice in a row, so the next change must not be made blind.

Nothing measures that 26 ms. Two candidates, currently indistinguishable:

1. genuine guest CPU work (Celeste game logic, MonoGame, Mono)
2. VM exit/entry overhead — 14.84M syscall dispatches over 70 s is 212k exits per second, of which scePthreadGetspecific alone is 10.88M. The round-trip cost is attributed to the guest-exec counter, NOT to the syscall counter, so the profiler currently hides it inside what looks like guest work.

Note guest exec reads 98.4% of wall while the flip is only 23.9 ms of a 50 ms frame — consistent with either explanation, which is exactly why it must be measured rather than argued.

Work:
- measure the wall time between consecutive flips on the guest thread that flips, and split it: time inside the flip syscall, time in other syscalls, time executing guest code
- attribute per guest thread, not process-wide sums — the current guest-exec and syscall totals sum across threads and exceed 100% of wall, which makes per-frame reasoning impossible
- measure the cost of ONE VM exit/entry round trip directly (this is task-207 AC #1; do it here since the number is needed now), so the 212k/s traffic converts into a real millisecond figure per frame instead of an inference
- report it as a frame-budget table in the same shape as the flip budget row, summing to the actual frame time with an explicit unaccounted percentage

House pattern as in task-203: relaxed AtomicU64 behind the existing UNEMUPS4_PROFILE gate, zero cost when unset, printed from app/unemups4/src/profiler_dump.rs, with matching tracing spans.

Success is the same arithmetic standard task-203 was held to: the printed frame budget must account for the measured frame time, leaving no multi-millisecond remainder. If the split turns out to contradict both hypotheses above, say so — that is the most valuable result this task can produce.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 per-frame wall time on the flipping guest thread is measured and split into flip syscall / other syscalls / guest code execution, attributed per thread rather than summed process-wide
- [x] #2 the cost of a single VM exit/entry round trip is measured directly, and the per-frame cost of the observed syscall rate is reported as milliseconds
- [x] #3 a frame-budget table sums to the actual frame time with an explicit unaccounted percentage, matching the standard task-203 met for the flip budget
- [x] #4 zero cost when UNEMUPS4_PROFILE is unset; build + clippy clean, cargo test --workspace green
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. crates/cpu/src/profile.rs: add per-thread frame accounting (thread-local accumulators + process-wide FrameStats), a settable flip-syscall-id list (ps4-cpu must not depend on ps4-syscalls), and a slot for the measured VM exit/entry round trip.
2. crates/cpu/src/exec.rs drive(): switch to a rolling timestamp so every nanosecond of a loop iteration is attributed (guest slice / pre-dispatch marshalling / dispatch / post-dispatch bookkeeping) with no gap; keep guest_ns + record_syscall semantics identical so the existing rows stay comparable. Guard with a nesting depth counter so call_guest recursion does not double-count.
3. Direct VM exit/entry measurement: write a 17-byte guest stub (mov eax,ID / syscall / dec rdi / jnz / ret) into a scratch guest page at boot, run it N times through run_guest_call with dispatch bypassed for that one magic id, and record (wall - dispatch)/N. Restore the counters afterwards so the calibration does not pollute the aggregate.
4. app/unemups4/src/main.rs: register the flip syscall ids and fire the calibration, both under the UNEMUPS4_PROFILE gate.
5. app/unemups4/src/profiler_dump.rs: print a windowed (delta-since-last-dump) frame-budget table on the flipping thread that sums to the measured frame time with an explicit unaccounted %, plus frames-per-window and the exit-overhead-per-frame figure derived from the calibration.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-21. Measured on the ATTRACT/menu scene (gameplay needs a pad the maintainer holds) — do not read these as gameplay numbers.

Frame budget now printed per dump window (delta since the previous dump), attributed to the flipping guest thread only:

  guest frame [tid 1] window: 252 frames, 25.26 fps — avg 39.589 ms = guest_exec 14.286 + flip 24.069 + other_syscalls 0.794 + run_loop 0.440 + unaccounted -0.000 (-0.0%)

Four consecutive windows all closed at 0.0% unaccounted (AC #3). The rows tile because the run loop now uses a rolling timestamp — guest slice / pre-dispatch marshalling / dispatch / post-dispatch bookkeeping share their boundaries, so no nanosecond falls between phases. The wall is measured independently (flip-return to flip-return), so the remainder is real.

VM exit/entry measured directly (AC #2): a guest stub (mov eax,ID / syscall / dec rdi / jnz / ret) is run 400k times at boot with the magic id answered inside the run loop, above the HLE dispatcher. 133 ns per round trip as the profiler sees it, of which 68 ns is the profiler's own four Instant::now() reads => 65 ns unprofiled. It is a floor: a tight loop keeps the translation cache hotter than a real title's scattered syscall sites.

THE RESULT, and it contradicts hypothesis 2 outright: the flipping thread issues ~4000 syscalls/frame, so the entire VM-exit tax is 0.53 ms/frame profiled, 0.26 ms/frame unprofiled — ~1% of the frame. The 14.3 ms outside the flip is genuine guest CPU execution. The 212k exits/s in the old dump are overwhelmingly on threads that do NOT gate the frame (scePthreadGetspecific is 31M calls at 73 ns = 2.3 s over 240 s of wall, spread across guest threads). A syscall RATE was never evidence of a frame cost. task-207 should be re-scoped or dropped on this evidence.

Instrumentation cost: nil. Frames per 10 s window before the change 253, after 252-256 — same scene, same fps.

Files: crates/cpu/src/profile.rs (pre/post-dispatch counters, FrameStats + per-thread FrameAcc, frame-boundary syscall registry, calibration module, restore/forget_syscall), crates/cpu/src/exec.rs (rolling-timestamp drive loop, DRIVE_DEPTH nesting guard, calibration short-circuit, calibrate_vm_exit), crates/cpu/src/guest_vm.rs (scratch page consts), app/unemups4/src/main.rs (register flip ids, fire calibration), app/unemups4/src/profiler_dump.rs (windowed frame-budget table).
<!-- SECTION:NOTES:END -->
