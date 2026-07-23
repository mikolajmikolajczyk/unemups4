---
id: TASK-213
title: >-
  diag: frame-time distribution — the profiler reports only averages, so felt
  stutter is unmeasurable
status: Done
assignee: []
created_date: '2026-07-21 20:09'
updated_date: '2026-07-21 22:01'
labels:
  - diag
  - perf
  - dx
dependencies: []
priority: medium
ordinal: 218000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every frame-timing row the profiler prints is a mean: gpu present avg ms/frame, flip budget avg, guest frame avg. A mean cannot distinguish steady 33 ms frames from an alternating 16/50 ms pattern, and those feel completely different.

This is not hypothetical. After task-208 raised Celeste to ~30 fps the maintainer reported the motion as mostly slow-motion (explained by decision-8, the clock) but with intermittent variance on top. The clock explains the slowness; nothing we currently print can confirm or deny the variance, so it cannot be investigated at all.

Add distribution, not just central tendency, to the existing per-frame counters:
- min, max, and p50/p95/p99 of frame time over the reporting window
- the same for the flip syscall, since it is the dominant per-frame component
- a count of frames exceeding some multiple of the target frame time, so a hitch shows as a number rather than as a feeling

Follow the house pattern (crates/gpu present_profile and the task-203 counters): relaxed atomics behind the existing UNEMUPS4_PROFILE gate, zero cost when unset. Percentiles need either a bounded histogram or reservoir — do NOT allocate per frame or take a lock on the frame path.

Also relevant to variance: the swapchain now uses MAILBOX (task-204), which discards an already-queued image when a newer one arrives. If frames are being dropped that is a real source of judder and should be countable here.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 min, max and p50/p95/p99 of frame time and of the flip syscall are reported per window, alongside the existing means
- [x] #2 frames exceeding a configurable multiple of the target frame time are counted and reported
- [x] #3 no per-frame allocation and no lock on the frame path; zero cost when UNEMUPS4_PROFILE is unset
- [x] #4 dropped/discarded presents under MAILBOX are counted if the swapchain path can observe them, or the notes state why not
- [x] #5 build + clippy clean, cargo test --workspace green
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. crates/cpu/src/profile.rs: add a bounded log2/8-sub-bucket ns histogram (256 relaxed AtomicU64 buckets, ~9% resolution) + exact min/max atomics, one for frame wall time and one for the flip syscall. Snapshot + per-window delta at dump time.
2. Same module: slow-frame attribution. A self-calibrating reference frame time (frugal median, single writer = the flipping thread; overridable with UNEMUPS4_PROFILE_TARGET_MS) and a threshold multiple (UNEMUPS4_PROFILE_SLOW, default 1.5). When a closing frame exceeds ref*mult, accumulate its task-209 phase split (guest_exec/flip/other_syscalls/run_loop/wall) into slow-only counters and bump a fixed 16-slot lock-free (id -> count, ns) table with the frame's longest single syscall. FrameAcc gains top_id/top_ns.
3. Same module: periodicity as a number. A 256-entry AtomicU32 ring of recent frame wall times in tenths of a ms, and inter-hitch gap stats (count/sum/min/max of the frame-index delta between consecutive slow frames) plus a 32-entry ring of the last gaps.
4. crates/gpu/src/present_profile.rs + backend.rs present(): inter-present interval on the display thread; count presents issued less than one 60 Hz vblank after the previous one — the necessary condition for a MAILBOX discard. Exact discard counts are NOT observable (queue_present returns no feedback and neither VK_KHR_present_wait nor VK_GOOGLE_display_timing is in use); document that in the module and in the task notes.
5. app/unemups4/src/profiler_dump.rs: new rows — frame time and flip distribution (min/p50/p95/p99/max), slow-frame count + share + phase split + top offending syscalls, hitch-gap stats, the recent-frame ring, and the present-interval row.
6. Verify: cargo build --release, clippy -D warnings, cargo test --workspace, a profiler-OFF boot, and a real UNEMUPS4_CLOCK=realtime UNEMUPS4_PROFILE=10 run on Celeste (attract/menu only - no gamepad available).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Symptom sharpened by the maintainer 2026-07-21, after the realtime clock (task-211) plus the pthread_cond_timedwait ABI fix (task-214) removed the half-speed defect:

Gameplay now runs at the correct SPEED but judders. The maintainer describes it as roughly 20 fast frames followed by 5 slow ones, nominally ~30 fps but visibly uneven. That is a PERIODIC pattern of about 25 frames, roughly 0.8 s at 30 fps — not random jitter. A periodic hitch has a cause with a period: a garbage collection, a buffer refill, a cache eviction, a resource reload.

Averages over a 10 s window cannot see this at all, which is exactly why the existing profiler rows are useless for it.

So percentiles alone are necessary but NOT sufficient — they would confirm the variance the maintainer already feels without explaining it. This task must also attribute it:

- when a frame exceeds a threshold multiple of the target frame time, record WHAT made it slow, using the per-phase split task-209 already computes (guest_exec / flip / other_syscalls / run_loop) plus the dominant syscall in that frame
- make the periodicity visible: either a compact recent-frame-time ring dumped on demand, or an inter-hitch interval statistic, so a ~25-frame period shows up as a number rather than as a feeling
- count MAILBOX discards if the swapchain path can observe them (task-204 made the present mode MAILBOX, which drops an already-queued image when a newer one arrives — a real judder source)

Hypotheses worth checking against the data, none of them assumed: Mono generation-0 GC (a managed-runtime title on a ~1 s cadence is the obvious first suspect), audio ring refill (sceAudioOutOutputs blocks against the DAC and is the top syscall by total time), and texture or shader cache invalidation on a periodic boundary.

Do not conclude from a plausible story. The instrument must NAME the cause.

---

## Implemented 2026-07-21

Rows added to the profiler dump, all behind the existing `UNEMUPS4_PROFILE` gate:

- `frame distribution (this window)` — min/p50/p95/p99/max of frame wall time and of the flip syscall. Backed by `profile::Dist`: a 512-bucket log2 histogram with 16 sub-buckets per octave (~6% bucket width, reaches 17 s) plus exact min/max/count/sum, all relaxed atomics. The dump keeps the previous window's bucket counts so each table is a delta, and *takes* min/max (swap) so they are per-window too.
- `slow frames` / `slow frame avg` — frames over `UNEMUPS4_PROFILE_SLOW` (default 1.5) x the reference frame time, with the task-209 phase split restricted to those frames. The reference is NOT a fixed target: a fixed 16.7 ms budget would flag every frame of a title running at 30 fps. It is a frugal running-median estimator updated by the flipping thread (one multiplicative step per frame), which a 20%-duty hitch tail cannot drag up. `UNEMUPS4_PROFILE_TARGET_MS` pins it instead when you want an absolute budget.
- `hitch period` + `recent hitch gaps` — consecutive slow frames collapse into one hitch; the gap between hitch *starts* is counted, min/max/mean'd, and the last 32 gaps are printed literally. A 25-frame period would appear as a column of 25s.
- `recent frame times` — a 256-entry lock-free ring of frame wall times in µs, printed 20 per line. This is what actually shows the shape.
- `longest syscall in each slow frame` — a fixed 16-slot lock-free `(id, count, ns)` table fed with the frame's longest single non-flip syscall. Cumulative, with an overflow counter.
- `present interval [window]` — see MAILBOX below.

No allocation and no lock on the frame path: everything is `AtomicU64`/`AtomicU32` arrays and two extra `Cell` fields in the existing thread-local `FrameAcc`. Verified with a profiler-OFF boot — no dump thread, no atexit, none of the atomics touched.

### MAILBOX discards: bounded, not counted

**Vulkan cannot report them.** `queue_present` returns `SUCCESS` whether the image reached the display or was replaced in the queue. The extensions that would tell us — `VK_KHR_present_wait` and `VK_GOOGLE_display_timing`'s `actualPresentTime` — are not enabled on this swapchain. What is observable is the *necessary condition*: MAILBOX can only drop an image when a newer one is queued before the next vblank, so the row counts presents issued less than one 60 Hz period after the previous one. Zero is a proof of absence; non-zero is an upper bound.

Measured: **1 of 579 in one window, 0 of 474, 0 of 505, 0 of 507.** MAILBOX discards are ruled out as a judder source at the rates we currently achieve.

### What the instrument shows — ATTRACT/MENU SCENE ONLY

No gamepad was available to the agent, so **none of this is gameplay**. Numbers from `UNEMUPS4_CLOCK=realtime UNEMUPS4_PROFILE=10`, two runs, steady state after ~60 s:

```
  guest frame [tid 1] window: 507 frames, 50.73 fps — avg 19.712 ms = guest_exec 13.457 + flip 5.807 + other_syscalls 0.208 + run_loop 0.239 + unaccounted 0.000 (0.0%)
  frame distribution (this window):
    frame      n=507   min  16.679 | p50  18.350 | p95  26.739 | p99  30.933 | max  33.672 ms
    flip call  n=507   min   5.211 | p50   5.636 | p95   6.685 | p99   7.209 | max   7.565 ms
    slow frames: 14 over 8 hitches — threshold 1.50 x 18.604 ms reference (running median)
    slow frame avg 30.389 ms = guest_exec 23.425 + flip 6.289 + other_syscalls 0.336 + run_loop 0.340 + unaccounted -0.000
    hitch period: 8 gaps this window, mean 68.6 frames (min 2 max 393 since start), burst 1.9 frames avg (max 5 since start)
    recent hitch gaps (frames): 110 2 37 56 38 6 48 41 56 40 4 76 32 92 35 31 8 45 2 4 30 21 11 85 70 37 84 75 105 12 38 128
    longest syscall in each slow frame (cumulative):
      sceGnmSetVsShader                        topped     26 slow frames (avg 0.010 ms)
      sceGnmUpdateVsShader                     topped     11 slow frames (avg 0.011 ms)
      sem_wait                                 topped      8 slow frames (avg 20.102 ms)
      sceGnmDrawIndexAuto                      topped      8 slow frames (avg 0.022 ms)
    recent frame times (ms, oldest first, 256 frames):
        ...
        18.1  18.0  17.3  16.9  16.8  22.8  31.1  28.8  19.6  23.1  26.3  21.9  18.8  17.0  18.7  16.9  18.8  16.7  16.8  18.7
        17.1  20.6  20.6  26.5  28.8  33.7  32.1  22.4  16.8  17.5  16.7  17.1  18.6  17.5  17.5  18.5  18.5  24.4  23.6  17.4
    present interval [window]: avg 19.712 ms (min 16.573 max 33.721) — 0 of 507 presents landed inside one 60 Hz period
```

What that says, and only that:

1. **The variance is real, and it has the maintainer's shape.** The ring shows runs of 3-7 consecutive elevated frames (`22.8 31.1 28.8 19.6 23.1 26.3 21.9`, `26.5 28.8 33.7 32.1 22.4`) separated by stretches of clean 16.7-18 ms frames. Burst length maxes at 5. That is "N fast frames then a few slow ones" — the felt pattern is in the data.
2. **It is NOT periodic on attract.** Inter-hitch gaps are 2..393 frames, irregular, mean drifting 30-70 between windows. Nothing resembling a fixed ~25-frame period. Either the periodicity is specific to gameplay, or the maintainer's "every 25 frames" is the perceptual reading of an irregular ~3% slow-frame rate.
3. **Slow frames are slow because guest code ran longer, not because anything blocked.** A slow frame averages 30.4 ms, of which `guest_exec` is 23.4 ms (77%) against 13.5 ms in an average frame. `flip` barely moves (6.3 vs 5.8 ms), `other_syscalls` is 0.34 ms, unaccounted is 0.000. The longest single syscall in most slow frames is a trivially cheap GNM call (`sceGnmSetVsShader`, 0.010 ms) — i.e. there was no blocking call at all; the syscall named is just the largest of many tiny ones.
4. **JIT recompilation is ruled out.** `x86jit compile_ns` is flat in steady state (10.42 s cumulative, +1..15 ms per 10 s window) while slow frames keep occurring at ~3%.
5. **MAILBOX is ruled out** (above). **Audio is not implicated**: `sceAudioOutOutputs` is the top syscall by total time but it runs on another thread and contributes 0.34 ms to a slow frame on the flip thread.

### Honest verdict

**The instrument bounds the cause; it does not yet name it.** It says the judder is extra *guest x86 execution on the flipping thread* — not the GPU, not the flip, not a blocking HLE call, not the presentation engine, not the JIT. Mono gen-0 GC remains the leading candidate precisely because it is guest code, but this instrument cannot distinguish a GC from any other in-guest work spike, and no evidence here elevates it above a hypothesis.

Two things are needed and are NOT in this task's scope:

- **A gamepad run.** The ~25-frame periodicity is a gameplay claim and gameplay was unreachable. The attract scene may simply not exercise whatever has the period.
- **Guest-side attribution inside `guest_exec`.** Naming the cause needs the guest RIP/symbol during a slow frame — the `UNEMUPS4_EXECTRACE` backtrace sampler triggered by the slow-frame predicate rather than by a timer. That is a separate task.
<!-- SECTION:NOTES:END -->
