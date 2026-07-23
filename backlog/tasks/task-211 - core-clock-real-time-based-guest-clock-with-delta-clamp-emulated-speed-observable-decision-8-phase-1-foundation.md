---
id: TASK-211
title: >-
  core/clock: real-time-based guest clock with delta clamp + emulated-speed
  observable (decision-8 phase 1, foundation)
status: Done
assignee: []
created_date: '2026-07-21 20:08'
updated_date: '2026-07-21 22:01'
labels:
  - core
  - clock
  - arch
dependencies: []
priority: high
ordinal: 216000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Implements decision-8 phase 1. Read backlog/decisions/decision-8 first — it carries the rationale, the rejected alternatives and the risk list, and this task is deliberately scoped to the FOUNDATION only.

Today crates/core/src/clock.rs RENDER phase returns anchor + flips x 16_666_667 ns, so guest world-time is a pure function of presented host frames. Measured consequence: Celeste asks for 60 Hz (sceVideoOutSetFlipRate rate=0, logged) and the host presents ~30 fps, so the game runs at exactly half speed — smooth, evenly paced, wrong. At 20 fps it was one third speed. The maintainer describes it as playable but in slow motion.

Scope of THIS task:
- virtual time advances with real host elapsed time, not with flips
- a max-delta clamp bounds any single advance to a few frames, preserving the anti-fast-forward property task-113 needed and task-169 extended; this clamp is the non-negotiable part of the design
- strict monotonicity with a per-read floor is preserved, so guest spin-waits on a changing clock still terminate — a previous flip-only-clock attempt deadlocked exactly there
- two modes, house-style env-gated like UNEMUPS4_BACKEND: realtime (new default) and fixed-step (today's behaviour, kept deliberately — headless oracle baselines and the PNG visual oracle depend on deterministic virtual time, and so does x86jit's constant rdtsc)
- emulated speed, d(virtual)/d(real) over a window, exposed as a percentage and printed in the UNEMUPS4_PROFILE table and the window title

OUT of scope, deferred to phase 2: honouring the flip rate and generating vblank from the time base. Do not start those here.

Consumers are few and all known: now_ns has three callers, all in crates/libs/src/libkernel/mod.rs (virtual_epoch_ns, sceKernelGetProcessTimeCounter, sceKernelGetProcessTime); advance_frame has one caller (crates/gpu/src/lib.rs); flip_count is used only for diagnostic and snapshot frame labelling and keeps its current meaning.

REGRESSION RISK IS THE POINT OF THIS TASK. Four closed fixes were tuned against the fixed-step clock: task-113 (splash fast-forward), task-157 (GPU completion timing and command-buffer recycle), task-169 (BOOT-phase per-read cap), task-170 (intro loop rewind). Any of them may have been leaning on it silently. Each scene must be checked before and after by the maintainer's eyes — a log or a frame counter is not evidence that a splash sequence still looks right. State plainly which scenes you could NOT verify yourself rather than implying they passed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 RENDER-phase virtual time advances with real elapsed host time, with a bounded max-delta clamp and preserved strict monotonicity plus per-read floor
- [x] #2 realtime and fixed-step modes selectable by env var, realtime default, fixed-step reproducing today's flips-times-fixed-step behaviour exactly
- [x] #3 emulated speed percentage is computed and printed in the profiler table and the window title
- [x] #4 measured: at the current host frame rate the guest's own elapsed time matches wall time within a few percent, where it previously ran at half — reported as before/after numbers
- [x] #5 build + clippy clean, cargo test --workspace green; the four regression scenes (task-113/157/169/170) explicitly listed as verified-by-maintainer or not-verified, never assumed
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. clock.rs rewrite: keep BOOT phase (per-read cap to one FRAME_NS, task-169) and the +1us monotonic floor untouched. RENDER phase gains two modes behind UNEMUPS4_CLOCK (realtime default | fixed-step).
2. realtime RENDER = frame-anchored real time: advance_frame() re-anchors (FRAME_REAL_ANCHOR = real_ns, FRAME_VIRT_ANCHOR = virtual now) on every flip; now_ns = virt_anchor + min(real_ns - real_anchor, MAX_FRAME_DELTA_NS). MAX_FRAME_DELTA_NS = 4 x FRAME_NS (66.7 ms) = the decision-8 max-delta clamp, applied at READ time (not retroactively at the next flip) so a hitch can never be observed as a fast-forward.
3. fixed-step RENDER = today's exact code path (anchor at first flip + FLIP_COUNT x 16_666_667), pinned by a test that reproduces the current assertions byte-for-byte.
4. Add non-mutating peek_ns() + a SpeedMeter (d(virtual)/d(real) over a caller-owned window, percent). Wire it into the window title (crates/gpu/src/display.rs, 1 Hz) and the profiler table (app/unemups4/src/profiler_dump.rs).
5. Measure emulated speed live on Celeste attract/menu in both modes; report before/after ratio.
6. build + clippy + cargo test --workspace; list task-113/157/169/170 scenes honestly as verified / needs-maintainer-eyes / not-verified.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
IMPLEMENTED 2026-07-21 (decision-8 phase 1 only; task-212 flip-rate/vblank and task-213 percentiles NOT started).

FILES: crates/core/src/clock.rs (rewritten), crates/gpu/src/display.rs (SpeedMeter + window title + stale 16.67ms-per-flip comment), crates/gpu/src/lib.rs:75 (flip = frame boundary, not a fixed time step), app/unemups4/src/profiler_dump.rs (emulated-speed row), crates/libs/src/libkernel/mod.rs (two stale doc comments).

DESIGN AS BUILT. BOOT phase and the +1us per-read monotonic floor are UNCHANGED (task-169 cap intact, verbatim). RENDER phase gains two modes behind UNEMUPS4_CLOCK: realtime (default) and fixed-step. realtime = frame-anchored real time: every flip re-anchors (REAL_ANCHOR=real_ns, VIRT_ANCHOR=virtual now) and a read returns VIRT_ANCHOR + min(real elapsed since anchor, MAX_FRAME_DELTA_NS = 4 x 16.67 ms = 66.7 ms). The clamp is applied ON THE READ, not retroactively at the next flip — the guest reads the clock DURING a slow frame, which is where a fast-forward would be observed. fixed-step = the previous anchor + flips x 16_666_667, byte-for-byte, pinned by test section (e). New peek_ns() reads virtual time without consuming the floor; SpeedMeter is a caller-owned d(virtual)/d(real) window so the 1 Hz window title and the 10 s profiler row do not consume each other's window.

MEASURED, same Celeste ATTRACT/MENU scene (NOT gameplay — gameplay needs a pad the maintainer holds; every number below is the attract/menu loop):
- BEFORE, UNEMUPS4_CLOCK=fixed-step, 5 consecutive 10 s windows: 46.63/46.22/46.79/46.71/46.42 fps -> 77.7/77.2/78.0/77.8/77.3 % guest time per real second.
- AFTER, realtime (default), 8 consecutive steady 10 s windows: 46.8-49.1 fps -> 100.0 % (one window 99.5, one 95.6).
- So one wall second produced 0.775 s of guest-visible time before and 1.000 s after, on the same scene at the same ~47 fps. At the maintainer's 30 fps the before number is 30/60 = 50 %.
- INSTRUMENT VALIDATED against an independent quantity: in fixed-step the reported speed equals fps/60 to the decimal in all 5 windows (46.63/60=77.7, 46.79/60=78.0, ...). That is the coupling decision-8 describes, measured.
- CLAMP OBSERVED WORKING: during a ~10 s load stall (fps collapsed to 8.8, other_syscalls 97 ms/frame, multi-second scePthreadCondTimedwait) realtime reported 16.5-20.5 %, i.e. virtual time deliberately fell behind wall time instead of fast-forwarding. Anti-fast-forward (task-113) holds.
- BOOT->RENDER window reports >100 % (152.7 realtime / 160.9 fixed-step) in BOTH modes: that is the documented BOOT catch-up (+FRAME_NS per read), pre-existing, not introduced here.

GREEN: cargo build --release clean; cargo test --workspace all green (2 new clock tests); cargo clippy --all-targets --all-features -D warnings clean for everything touched — the only 4 errors are PRE-EXISTING in crates/gnm/src/shader/gcn.rs:985/1060/1174/1201 (redundant pattern matching), untouched. cargo fmt clean for all touched files (gcn.rs:982 remains the repo's only fmt diff, pre-existing).

REGRESSION SCENES — HONEST STATUS, NONE OF THESE WERE VISUALLY VERIFIED BY THE AGENT:
- task-113 splash fast-forward: NOT VERIFIED. Cannot be judged from a log. The clamp that task-113 needed is preserved but its budget changed from 16.67 ms/flip to <=66.7 ms/frame, so on a slow first-frames stretch the splash can now advance up to 4x faster than task-169 tuned it. NEEDS MAINTAINER EYES — highest-risk item of this change. If it fast-forwards again, lower MAX_FRAME_DELTA_NS (2 frames) rather than reverting the mode.
- task-157 GPU completion timing / command-buffer recycle: NOT VERIFIED visually. Indirect evidence only: 4 minutes of sustained ~47 fps with a stable per-frame budget and no submit/flip anomaly in the profiler. That is not a texture-correctness oracle. NEEDS MAINTAINER EYES.
- task-169 BOOT-phase per-read cap: VERIFIED BY AGENT at the code and test level — the BOOT branch is unchanged and the cap/catch-up assertions are re-run in both modes (test sections a-c, f). NOT verified as a warm-up bind-count match against real HW (that needs the frame0 2-binds/10-draws comparison, not re-run here).
- task-170 intro loop / rewind: NOT VERIFIED. Task is still In Progress and its root cause is open; a periodic intro replay is exactly the class of symptom a time-base change can move in either direction. NEEDS MAINTAINER EYES.

KNOWN, NOT FIXED HERE: decision-8 section 5 — x86jit lifts rdtsc to a fixed constant (x86jit-core/src/lift/mod.rs:967). A guest measuring elapsed time via TSC bypasses this clock entirely. Not hit by Celeste; would be an x86jit-backlog change, never edited from this repo.
POSSIBLE FOLLOW-UP (decision-8 already predicted it): sceAudioOutOutputs is the top syscall by total time and averages ~19 ms/call. With the world now running at real speed the audio/video relationship changed; if audio buffering becomes the frame-rate limiter that is a separate task, not this one.
<!-- SECTION:NOTES:END -->
