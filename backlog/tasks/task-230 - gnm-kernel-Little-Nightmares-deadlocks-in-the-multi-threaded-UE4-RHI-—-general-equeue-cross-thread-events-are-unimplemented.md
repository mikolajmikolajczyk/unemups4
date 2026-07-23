---
id: TASK-230
title: >-
  gnm/kernel: Little Nightmares deadlocks in the multi-threaded UE4 RHI —
  general equeue cross-thread events are unimplemented
status: To Do
assignee: []
created_date: '2026-07-23 07:00'
labels:
  - retail
  - gnm
  - kernel
  - little-nightmares
dependencies: []
ordinal: 235000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Little Nightmares (CUSA05952, UE4) boots through its whole import wall and renders 2-7 frames, then deadlocks. The stall-diagnosis toolbox (doc-4 §3.10) traces it: the root is SubmitGPUCommandsAsyncTaskThreadPS4 (tid 28) blocked in sceKernelWaitEqueue while holding the engine's GPU lock (mutex 0x4d72d78); SubmitDoneAsyncTaskThreadPS4 and RHIThread starve on that lock, and the whole TaskGraph pool blocks behind them.

The wait never wakes because the equeue it waits on has no GNM completion event and we deliver nothing else. Proven behaviourally: making a GNM-event wait always-trigger (sync executor => completion always ready) did NOT break the deadlock, so tid 28's stuck wait is on a NON-GNM equeue — an inter-thread event another RHI thread is supposed to post. Our equeue (doc-6 Entry 2, Phase A) models ONLY sceGnmAddEqEvent GPU completion; the general kqueue mechanism is unimplemented: sceKernelAddUserEvent / sceKernelTriggerUserEvent / sceKernelAddTimerEvent have no handlers, so a thread that waits for an event another thread triggers blocks forever.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 sceKernelAddUserEvent / sceKernelTriggerUserEvent deliver a cross-thread event: a thread waiting in sceKernelWaitEqueue for a user event wakes when another thread triggers it
- [ ] #2 equeue completion is per-queue, not a single global counter — a wait on one queue is not starved by a drain on another
- [ ] #3 Little Nightmares advances past the 2-7 frame RHI deadlock (SubmitGPUCommandsAsyncTaskThread no longer blocks forever holding the GPU lock)
- [ ] #4 Celeste unaffected: still 53-58 fps with correct textures (visual oracle)
<!-- AC:END -->
