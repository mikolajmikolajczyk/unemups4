---
id: TASK-205
title: >-
  gpu: per-submit wait_for_fences serializes guest and GPU — no frame pipelining
  at all
status: To Do
assignee: []
created_date: '2026-07-21 18:28'
labels:
  - gpu
  - perf
  - vulkan
dependencies:
  - TASK-203
priority: high
ordinal: 210000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/gpu/src/backend.rs:1449 blocks on wait_for_fences immediately after queue_submit, once per submitted draw list. The guest thread is blocked for that entire time inside GpuManager::run_command_list (crates/gpu/src/lib.rs:128-134), which sends the command list over the display channel and waits on a reply channel.

The result is a fully serial chain with zero overlap: guest builds the list, blocks; display thread records it; GPU executes it; display thread waits for completion; only then does the guest resume and reach its flip. CPU and GPU are never busy at the same time, so the frame cost is the SUM of both instead of the max.

The synchronous wait is load-bearing today, not incidental — the code after it reads back render-target layouts and the timeout path rolls current_layout back to UNDEFINED on a faulted submit (backend.rs:1452-1462), and doc-6 records that GPU completion timing already broke the guest's command-buffer recycle once before (task-157, the Celeste logo fix: a synchronous instant EOP-fence write made the guest skip its per-frame rebind). So this must be pipelined deliberately, with the layout tracking and the fence-timeout rollback made correct across frames, not merely deleted.

Depends on task-203: land the instrumentation first so the actual fence-wait share of the frame is measured rather than inferred (it currently sits inside an unmeasured ~24.7 ms). Do not start this before those numbers exist.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the per-submit fence wait no longer blocks the guest thread for the GPU's execution time; CPU and GPU work overlap across frames
- [ ] #2 render-target layout tracking and the fence-timeout rollback remain correct with submits in flight — no validation errors under a validation-layer run
- [ ] #3 measured before/after per-frame improvement recorded in the notes, using the task-203 counters
- [ ] #4 no regression in the guest's command-buffer recycle (the task-157 failure mode); maintainer confirms the scene still renders correctly on screen
<!-- AC:END -->
