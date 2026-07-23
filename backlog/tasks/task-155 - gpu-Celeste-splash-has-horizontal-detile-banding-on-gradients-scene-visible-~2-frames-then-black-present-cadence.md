---
id: TASK-155
title: >-
  gpu: Celeste splash has horizontal detile banding on gradients + scene visible
  ~2 frames then black (present cadence)
status: Done
assignee: []
created_date: '2026-07-17 07:30'
updated_date: '2026-07-17 08:35'
labels:
  - gpu
  - gnm
  - celeste
  - retail
  - bug
dependencies: []
priority: high
ordinal: 161000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After the alpha-blend fix (task-154, merge dad45a4) Celeste's 'Matt Makes Games Inc.' splash renders in correct warm pink/orange color (PNG oracle confirmed). Two residual artifacts remain: (1) HORIZONTAL SCANLINE BANDING across the smooth gradient — a detiling artifact (the scene RT / sampled surface tiling not fully inverted; relates the tile_idx=10/14 macro-tile handling vs our linear/linear-aligned/Thin1d detilers in crates/core/src/tiling.rs + crates/gnm/src/cache/tile.rs). (2) The composited scene is visible only ~2 frames (5-6) then the presented frame goes BLACK — a present/scanout CADENCE issue (double-buffer flip index alternation, or the scene draws only land on one buffer, or a per-frame clear/latch). Method: PNG oracle across many frames (orchestrator Reads); check the videoout target image tiling + the flip buf_idx alternation + any per-frame clear of texture_image. Relates task-153/154 (colored-frame goal achieved), task-56 (tiling).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused the horizontal banding (which tiling/detile step) + the ~2-frame-then-black cadence
- [x] #2 Fix: PNG oracle shows a clean gradient (no banding) AND the scene persists across frames
<!-- AC:END -->
