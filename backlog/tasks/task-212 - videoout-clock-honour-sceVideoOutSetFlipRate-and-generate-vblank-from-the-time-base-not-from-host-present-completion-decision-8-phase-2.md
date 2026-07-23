---
id: TASK-212
title: >-
  videoout/clock: honour sceVideoOutSetFlipRate and generate vblank from the
  time base, not from host present completion (decision-8 phase 2)
status: To Do
assignee: []
created_date: '2026-07-21 20:08'
labels:
  - core
  - clock
  - videoout
  - arch
dependencies:
  - TASK-211
priority: medium
ordinal: 217000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Implements decision-8 phase 2, after task-211 lands the time base. Read backlog/decisions/decision-8 first.

Two coupled problems remain after phase 1:

1. sceVideoOutSetFlipRate is a stub that discards its rate argument (crates/libs/src/libscevideoout/mod.rs). The rate is the only statement the guest makes about its intended display cadence — 0 = 60 Hz, 1 = 30 Hz, 2 = 20 Hz. Celeste sends 0, which is how we established it is a 60 Hz title rather than a natively-30 one.

2. The guest's flip events and flip status are satisfied by our host finishing a present. That means a host which cannot keep up throttles guest logic instead of dropping frames. Phase 1 fixes what time the guest reads; it does not fix what gates the guest's frame loop.

Work: derive a periodic vblank signal from the phase-1 time base at the guest-requested rate, and satisfy sceVideoOutGetFlipStatus / the flip equeue from it rather than from present completion. A slow host then loses frames, which is what real hardware does, instead of slowing the world.

Note the interaction with the existing real-time frame limiter in crates/gpu/src/display.rs (task-163): it paces flips to 60 Hz at the SubmitFlip choke point and can only ever delay a flip running ahead. Once vblank is generated from the time base, that limiter and the generator must not be two competing pacers — decide which one owns cadence and say so in the notes.

Same regression exposure as task-211: task-113/157/169/170 were all tuned around flip-driven timing, and task-157 in particular is about GPU completion timing breaking the guest's command-buffer recycle. Maintainer's eyes on those scenes, not reasoning.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 sceVideoOutSetFlipRate honours the requested rate (0/1/2 mapping to 60/30/20 Hz) instead of discarding it
- [ ] #2 flip status and flip events are satisfied from a vblank signal derived from the time base, not from host present completion
- [ ] #3 a host slower than the requested rate drops frames while guest world-time continues at the correct rate — demonstrated with a measurement, not asserted
- [ ] #4 the task-163 frame limiter and the vblank generator do not fight for cadence; which one owns it is stated in the notes
- [ ] #5 build + clippy clean, cargo test --workspace green; task-113/157/169/170 scenes listed as verified-by-maintainer or not-verified
<!-- AC:END -->
