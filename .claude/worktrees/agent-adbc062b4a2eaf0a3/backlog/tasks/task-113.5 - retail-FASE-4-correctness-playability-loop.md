---
id: TASK-113.5
title: 'retail FASE 4: correctness + playability loop'
status: To Do
assignee: []
created_date: '2026-07-14 08:28'
labels:
  - retail
  - gpu
  - hle
  - perf
dependencies:
  - TASK-113.4
parent_task_id: TASK-113
priority: medium
ordinal: 117000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
FASE 4 (parent epic). With a first frame + audio (FASE 3), iterate to playable: render correctness (real shaders, textures/tiling, render targets, blend/depth state the engine actually uses), audio correctness, input mapping, frame pacing; fix crashes, wrong pixels, missing features surfaced by running real content. METHOD: visual/audio oracle (PNG dumps by eye — no reference build available), differential where feasible, one task per defect. Perf pass (JIT tier-up) once correct, not before. Milestone-gated: pull-driven, do not pre-file speculative defects. Boundary: no crypto; assets local + gitignored; SPIR-V portable.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 rendering is correct for the content exercised (no gross artifacts; textures/RT/state right)
- [ ] #2 audio + input + frame pacing are correct enough to play
- [ ] #3 a perf pass (JIT tier-up) lands only after correctness holds
<!-- AC:END -->
