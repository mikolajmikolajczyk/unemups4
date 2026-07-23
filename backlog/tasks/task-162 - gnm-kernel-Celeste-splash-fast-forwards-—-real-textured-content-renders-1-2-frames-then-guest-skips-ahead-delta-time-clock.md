---
id: TASK-162
title: >-
  gnm/kernel: Celeste splash fast-forwards — real textured content renders 1-2
  frames then guest skips ahead (delta-time / clock)
status: Done
assignee: []
created_date: '2026-07-17 12:27'
updated_date: '2026-07-17 13:12'
labels:
  - gnm
  - kernel
  - celeste
  - retail
  - timing
dependencies: []
priority: high
ordinal: 168000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
PNG-oracle (task-157) PROVED Celeste's textures decode correctly and reach the screen: the 'Matt Makes Games presents' logo (1500x199) and the dark-navy mountain bg (4096x820) are real, correctly-decoded, and videoout-bound. But the logo binds only ~2x across a 90s run (would be thousands at 60fps) => the guest's splash state-machine advances after ~1-2 frames and the game runs ahead, so the correct content only flashes and the presented frame degenerates to dummy-dominated gradient squares. Root suspect: a huge/wrong first-frame delta-time or wrong clock source feeding the guest's Update loop (guest thinks seconds elapsed per frame). Investigate: which clock the guest reads for frame dt (sceKernelGetProcessTime / rdtsc / sceKernelClockGettime / videoout vblank pacing), whether the first-frame dt is enormous (boot took ~28s in guest time before the first Update), and whether we advance the guest clock in lockstep with presented frames. Fix so dt is sane (cap first dt, or pace to vblank) => splash holds, logo+mountain+clouds visible. Relates task-157 (textures proven working), doc-6. Method: log guest dt per Update + the clock reads; PNG oracle to confirm the splash holds.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Identified the clock/dt source that makes the guest fast-forward the splash
- [ ] #2 Splash holds long enough that the logo + mountain background are visible on the presented frame (PNG oracle)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
CONFIRMED root: all guest clocks (sceKernelGetProcessTime :513, GetProcessTimeCounter :499, gettimeofday :83, clock_gettime :454 in libs/libkernel/mod.rs) back off REAL host wall-clock (process_start Instant :489). Emulation ~1fps => guest measures ~1s real per rendered frame => FNA/MonoGame fixed-timestep runs many catch-up Updates per Draw (or variable-dt jumps ~1s/frame) => splash logic done in 1-2 rendered frames (logo bound only x2/90s). FIX: virtual/emulated guest clock advancing a fixed ~16.667ms (60Hz) per presented flip (submit_flip, core/videoout.rs), decoupled from real time; back all four clock HLEs off it. Agent: (1) CONFIRM numbers first (log real dt/flip + flip count over splash), (2) implement minimal virtual clock (AtomicU64 in ps4_core, bump per flip), (3) PNG-oracle: dump swapchain frames across first ~10s, orchestrator reads to verify logo/mountain HOLD. Watch for spin-wait-on-clock hangs (per-flip-only clock is constant within a frame).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
FIXED + MERGED 7bcf709 (fix 0656d9e). Virtual 60Hz guest clock (ps4_core::clock): BOOT=real time (spin-waits terminate), RENDER=first-flip anchor + flips*16.67ms, always monotonic. Wired at GpuManager::submit_flip + 4 time HLEs. RESULT (PNG oracle, orchestrator read frames 0/10/45/122): Celeste now HOLDS its splash across 123 presented frames (was 1-2), game alive/looping, no fast-forward, no boot hang. Build+58 core/libs tests green. AC#1 DONE. AC#2 (logo+mountain VISIBLE) NOT met — but that's a SEPARATE bug now cleanly isolated: the held splash still presents white-dummy gradient content (a ~1500x199 gradient bar = the logo quad rendered white-dummy instead of the logo texture, + gradient particle squares). Real logo/mountain atlases upload (task-157) but do NOT reach the PRESENTED draws — filed as follow-up. First Fable attempt's predecessor (opus) deadlocked on a flip-only clock at SystemService boot (spin-wait on frozen clock); the hybrid boot=real-time phase fixes that.
<!-- SECTION:NOTES:END -->
