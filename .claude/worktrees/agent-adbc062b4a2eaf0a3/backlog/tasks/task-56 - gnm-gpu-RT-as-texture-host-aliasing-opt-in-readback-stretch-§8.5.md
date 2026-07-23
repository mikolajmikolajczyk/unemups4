---
id: TASK-56
title: 'gnm/gpu: RT-as-texture host aliasing + opt-in readback (stretch, §8.5)'
status: To Do
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-11 12:56'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-53
  - TASK-51
priority: medium
ordinal: 55000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Closes §8.6 phase-4 cache row: arbitrary render targets become cache entries (RenderTarget{…} keys, backend render-to-texture); when a draw samples a range a prior draw rendered, resolve host-side via backend blit/alias — no guest copy; readback(target) backend method + ReadbackPolicy env lever UNEMUPS4_RT_READBACK (default Off), re-tiling on readback reusing P4-15 inverse. Last cache step (buffers→textures→RT-readback); can slip to 4.x without blocking milestones. Does NOT add per-title config.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: overlap detection — RT key + texture key over same range → exactly one host-side resolve command, zero guest writes
- [ ] #2 headless: policy Off → no readback commands; On → flagged RT emits readback + marks entry clean
- [ ] #3 live GPU: two-pass corpus (render-to-target, sample it) displays correctly
<!-- AC:END -->
