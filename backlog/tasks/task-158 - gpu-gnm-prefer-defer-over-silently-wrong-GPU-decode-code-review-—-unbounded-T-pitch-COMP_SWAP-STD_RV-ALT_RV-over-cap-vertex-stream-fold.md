---
id: TASK-158
title: >-
  gpu/gnm: prefer defer over silently-wrong GPU decode (code-review) — unbounded
  T# pitch, COMP_SWAP STD_RV/ALT_RV, over-cap vertex stream fold
status: To Do
assignee: []
created_date: '2026-07-17 11:32'
labels:
  - gpu
  - gnm
  - gcn
  - review
  - robustness
dependencies: []
priority: medium
ordinal: 164000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review of the color-pipeline session found three spots that ship silently-wrong pixels/geometry instead of deferring (the repo's strict-or-defer discipline): (1) crates/core/src/tiling.rs linear_aligned_pitch_or accepts ANY decoded pitch>=width verbatim (raw 14-bit guest field, no upper bound/alignment) — a garbage word4 yielding a huge-but->=width pitch strides detile at that pitch (sheared texels or a ShortBuffer defer); add a sanity clamp/upper bound. (2) crates/gnm/src/derive.rs color_format maps COMP_SWAP STD_RV(2)/ALT_RV(3) (alpha-first reversed layouts) to the nearest non-reversed base order, silently dropping the reversal — unlike an unsupported FORMAT it does NOT defer; either model the reversed orders or return None to defer. (3) crates/gcn/src/recompile.rs ensure_vs_buffer folds a 5th+ distinct vertex V# onto the last stream (wrong buffer/num_records/dst_sel) instead of deferring — a >MAX_VS_STREAMS(4) VS renders corrupt geometry with no diagnostic; defer instead of mis-fetch. All three are dormant for Celeste but latent for other titles.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 pitch bounded/sanity-checked; COMP_SWAP RV either modeled or deferred; over-cap vertex stream defers instead of mis-fetching
<!-- AC:END -->
