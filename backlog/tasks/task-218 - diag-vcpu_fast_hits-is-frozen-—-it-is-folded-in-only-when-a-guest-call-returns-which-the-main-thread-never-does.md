---
id: TASK-218
title: >-
  diag: vcpu_fast_hits is frozen — it is folded in only when a guest call
  returns, which the main thread never does
status: Done
assignee: []
created_date: '2026-07-22 08:38'
updated_date: '2026-07-22 08:45'
labels:
  - diag
  - cpu
  - perf
dependencies: []
priority: high
ordinal: 223000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cpu/src/exec.rs:530 folds a vcpu's fast-resolve cache hits into the process total exactly once, on the way out of run_guest_call:

    if profiling { profile::EXEC.vcpu_fast_hits.fetch_add(cpu.fast_hits(), Ordering::Relaxed); }

The comment explains the choice — the counter is per-vcpu and not atomic in x86jit, so it is read on the single exit path rather than at every return. The consequence was not noticed: the main guest thread runs the entire game inside one run_guest_call and never returns, so its hits are never folded in. The printed total therefore reflects only threads that have already exited, and sits frozen for the rest of the run.

Observed in a Celeste gameplay run, three consecutive 10 s windows while the game was plainly executing:

    vcpu_fast_hits=9482453
    vcpu_fast_hits=9482453
    vcpu_fast_hits=9482453

Identical to the unit. Meanwhile hits climbed 32.9M -> 34.5M and chained 5.69G -> 6.28G in the same windows, so the emulator was very much running.

This now blocks a measurement we are about to need. x86jit task-278 (N-way IBTC) reports its effect exactly through this counter — its own A/B shows indirect fast_hits going 0 -> 937449, a 94% hit rate on a megamorphic site, with indirect-call time down 40%. When that lands and the rev pin is bumped, we cannot confirm the win on Celeste with a counter that does not move.

Fix: sample cpu.fast_hits() from the owning thread at a point that recurs, rather than only at exit. The frame boundary on the flipping thread is the natural place — per-frame bookkeeping already happens there (task-209/213) and the read stays on the vcpu's own thread, which is what the non-atomic per-vcpu counter requires. Report a per-window delta like the other frame rows rather than a running total, so a stalled counter is visible as a zero instead of hiding behind a large cumulative number.

While there, audit the other counters folded on the same exit path for the same defect — any statistic that only accumulates when a guest call returns is invisible for the thread that matters most.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 fast-resolve hits are sampled at a recurring point on the owning thread, not only when a guest call returns, so the main guest thread is included
- [x] #2 the profiler reports a per-window delta, so a counter that stops moving is visible rather than masked by a cumulative total
- [x] #3 any other counter folded only on the run_guest_call exit path is audited and either fixed or documented as intentionally exit-only
- [x] #4 verified against a running Celeste session: the value advances between windows; zero cost when UNEMUPS4_PROFILE is unset
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Audit crates/cpu/src/exec.rs for every counter folded only on the run_guest_call exit path (fast_hits today; check retired_instructions and anything else read from the vcpu). 2. Sample the vcpu's fast_hits from its OWN thread at the per-frame boundary that task-209 already established, storing the last-seen value so the fold is a delta rather than a re-add. 3. Keep the exit-path fold for threads that do return, without double counting. 4. Print a per-window delta row instead of a running total so a stalled counter shows as zero. 5. Verify against a live Celeste run that the value advances between windows.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fixed 2026-07-22.

CAUSE: crates/cpu/src/exec.rs folded cpu.fast_hits() into the process total exactly once, on the way out of run_guest_call. The main guest thread runs the entire title inside one such call and never returns, so its hits were never counted; the printed total reflected only threads that had already exited and then sat frozen. Observed frozen at 9482453 across three consecutive 10 s windows while hits climbed 32.9M -> 34.5M.

FIX: fold DELTAS from two places, tracking what has already been folded in a local (folded_fast_hits):
- exec.rs:455 — at each frame boundary, on the vcpu's own thread (Vcpu::fast_hits is per-vcpu and not atomic in x86jit, so the read must stay on that thread). is_frame_boundary was made pub(crate) for this.
- exec.rs:548 — the existing exit path now adds only the remainder since the last boundary fold. Still required: a thread that never reaches a frame boundary (worker, TLS destructor, nested call_guest) reaches only this path, and the flipping thread has hits after its final boundary. Subtracting what was already folded is what stops the two paths double counting.

REPORTING: profiler_dump.rs prints a per-window delta beside the cumulative total. A cumulative number hides a stalled counter behind a large value, which is exactly how this stayed unnoticed; a +0 in a live run is now visible on sight.

AUDIT (AC #3): fast_hits was the only vcpu statistic folded on the exit path. x86jit also exposes Vcpu::retired_instructions(), which this repo does not read anywhere, so there is nothing else to fix or to document as intentionally exit-only.

VERIFIED against a live Celeste session — the counter advances between windows where it previously did not:
    vcpu_fast_hits=2          (+2 this window)
    vcpu_fast_hits=4252712    (+4252710 this window)
    vcpu_fast_hits=4694496    (+441784 this window)
    vcpu_fast_hits=13788135   (+9093639 this window)
Build clean, clippy -D warnings clean on ps4-cpu and unemups4, cargo test --workspace 575 green, cargo fmt clean apart from the pre-existing gcn.rs diff.

WORTH NOTING for x86jit task-278 (N-way IBTC): the first window shows the flipping thread's vcpu at just 2 fast hits while the game is already running. If that holds up under a gameplay run, it matches the megamorphic-site profile task-278 targets, where the current IBTC never hits at all (its own A/B moved indirect fast_hits 0 -> 937449). This counter is now able to measure that.
<!-- SECTION:NOTES:END -->
