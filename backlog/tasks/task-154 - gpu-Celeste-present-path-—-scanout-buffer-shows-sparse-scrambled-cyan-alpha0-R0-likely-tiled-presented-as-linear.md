---
id: TASK-154
title: >-
  gpu: Celeste present-path — scanout buffer shows sparse/scrambled cyan
  (alpha=0, R=0, likely tiled-presented-as-linear)
status: Done
assignee: []
created_date: '2026-07-17 06:41'
updated_date: '2026-07-17 07:30'
labels:
  - gpu
  - gnm
  - celeste
  - retail
  - bug
dependencies: []
priority: high
ordinal: 160000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After the videoout direct-scanout fixes (task-153, merge ad7bb19), Celeste's white-on-black bug is gone and colored content reaches the scanout buffer, but the PRESENTED frame is not yet a clean scene: PNG oracle shows only sparse scattered cyan (0,255,255) + a few green pixels (2845 of 2M) on black, with alpha=0 on every pixel and R=0 in ALL pixels (only G/B channels carry data), and frames 550-752 are byte-identical (static). Three intertwined present-path defects: (a) alpha=0 everywhere -> the present blit / swapchain composites the frame transparent (fix: force alpha=1 in the present blit, or handle the scanout's alpha/pixel-format); (b) R=0 in all pixels = a channel/pixel-format order mismatch (the scanout pixelFormat from SetBufferAttribute, e.g. 0x80000000 A8R8G8B8/B8G8R8A8, vs how the present path reads it); (c) sparse/scattered content strongly implies the scanout buffer is TILED (tile_idx=14 observed) but the present path reads/blits it as LINEAR -> scramble -> only stray pixels survive; needs the macro-tile detile on the present/readback path (the tile_idx>=9 Macro2d detiler we defer for textures, applied scanout-side). Method: PNG oracle (orchestrator Reads); isolation probes; check the scanout image's vk format + the present blit in crates/gpu/src/backend.rs (present/copy path) + the scanout buffer's tiling from the registered attribute. Relates task-153 (parent color goal), task-56 (RT/tiling), the linear-aligned + Macro2d tiling work in crates/core/src/tiling.rs + crates/gnm/src/cache/tile.rs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused why the presented scanout content is sparse/scrambled with alpha=0 and R=0 (tiling vs format vs blit)
- [x] #2 Fix: PNG oracle shows Celeste's colored scene (splash/loading) presented correctly (not sparse cyan, not transparent)
<!-- AC:END -->
