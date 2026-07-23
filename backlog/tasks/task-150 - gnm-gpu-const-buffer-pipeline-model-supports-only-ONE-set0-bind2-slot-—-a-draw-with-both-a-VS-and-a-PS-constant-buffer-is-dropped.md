---
id: TASK-150
title: >-
  gnm/gpu: const-buffer pipeline model supports only ONE set0/bind2 slot — a
  draw with both a VS and a PS constant buffer is dropped
status: To Do
assignee: []
created_date: '2026-07-16 16:40'
labels:
  - gnm
  - gpu
  - gcn
  - retail
dependencies: []
priority: medium
ordinal: 156000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding (2026-07-16). exec.rs derive_draw_state harvests the constant buffer from whichever stage declares one (VS|PS, task-139), but the pipeline protocol carries a SINGLE const_storage descriptor at the hardcoded set0/bind2. When BOTH the VS and the PS declare a constant buffer, the executor DEFERS the whole draw (they'd collide on the one slot) — a debug log, no geometry. Behavior-identical for Celeste (one CB, one stage) but a latent render wall: a title whose VS reads a transform-matrix CB AND whose PS reads a material-constants CB (at distinct set/binding) loses ALL its draws. The single-CB-per-stage design is a corpus shortcut elevated to a structural constraint. Fix: model TWO independent const_storage slots (one per stage), each carrying its own set/binding + stage_flags, mirroring the real GNM pipeline layout — replace the const_storage/const_storage_fragment single-slot+bool (core/gpu.rs CreatePipeline) with per-stage slots, populate both in setup_draw, declare both in the backend descriptor-set layout. Also generalizes the const_storage_fragment bool (a per-slot Stage/ShaderStageFlags is deeper than a VS-vs-PS bool). Relates to task-130 (ResourceSignature) + task-139.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A draw where BOTH VS and PS declare a constant buffer binds BOTH (no defer); each descriptor gets the correct per-stage stage_flags + set/binding
- [ ] #2 const_storage_fragment bool replaced by per-slot stage; headless test with a VS-CB + PS-CB at distinct bindings records both descriptors + renders
<!-- AC:END -->
