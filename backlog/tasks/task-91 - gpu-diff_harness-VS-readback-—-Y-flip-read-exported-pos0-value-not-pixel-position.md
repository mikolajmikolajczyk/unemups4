---
id: TASK-91
title: >-
  gpu: diff_harness VS readback — Y-flip + read exported pos0 value (not pixel
  position)
status: Done
assignee: []
created_date: '2026-07-12 14:06'
updated_date: '2026-07-13 20:11'
labels:
  - gpu
  - gcn
dependencies: []
priority: low
ordinal: 90000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-41 GPU-tier (diff_harness) VS path DIVERGES on a maintainer run 2026-07-12 — but it is a HARNESS READBACK artifact, NOT a recompiler bug (the recompiler is validated: flat_color_ps + interp_color_ps PS mrt0 == oracle EXACT on real GPU; the VS triangle renders at the correct NDC positions). Two readback defects in crates/gpu/src/bin/diff_harness.rs VS path: (1) Y-FLIP — clip_to_pixel maps clip-Y without the viewport Y-flip, so vertex 0 (top, clip y=+1) is read at pixel y=63 (bottom) → an uncovered/cleared [0,0,0,0] pixel; flip ndc_y in clip_to_pixel. (2) READS PIXEL POSITION NOT THE EXPORTED VALUE — the GPU readback returns pixel-center NDC coords (e.g. -0.984375 = -63/64 = pixel(0,0) center) instead of the VS's exported pos0 value, so it can never match within eps 1e-5 (pixel quantization = 1/64 = 0.0156 >> eps). FIX: read the VS's exported VALUE via a varying — passthrough_vs exports param0 (a Location output) = the same v[4:7] as pos0; write that interpolated Location output into the color target and sample at the vertex's pixel, OR render points and read the exact per-vertex value, so the readback is the shader's computed value not the rasterized position. Then VS validates cleanly on GPU like PS already does. LOW — the recompiler is already GPU-confirmed for PS + VS-renders-correctly; this only makes the VS tier-b readback exact.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 diff_harness VS path reads the shader's exported pos0 value (via a Location varying), not the rasterized pixel position; passthrough_vs VS output matches the oracle within eps on a maintainer GPU run
- [x] #2 clip_to_pixel Y-flip corrected (or documented as driver-dependent); no [DIVERGE] on the corpus VS for a correct recompile
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Flat-varying readback (option c): render one triangle per vertex (repeated SSBO rotation, that vertex provoking), companion FS reads Location=0 FLAT so every fragment carries the provoking vertex's exact exp pos0; NaN-clear + scan first covered texel → exact per-vertex value. Drops clip_to_pixel/covered_texel_near (the bug class) + their tests. No PointSize, no recompiler change, no SPIR-V surgery.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-13. Flat-varying readback: one triangle per vertex (SSBO rotation, provoking vertex), FS Location=0 Flat, NaN-clear + first-covered scan → exact per-vertex exported value. Removed clip_to_pixel/covered_texel_near + 4 tests (the bug class); added first_covered_texel + 2 tests. GPU-verified: [ok] passthrough_vs 3 vertices == oracle, all PS paths still [ok]. No recompiler change, no PointSize, no SPIR-V surgery.
<!-- SECTION:NOTES:END -->

## Notes

PARTIAL (merged @<prior-history> — an improvement, not complete). Live diff_harness re-run 2026-07-12 (maintainer): vertex 0 NDC(0,+1)→bottom-center now [ok] (POINT_LIST fixed the interpolation-quantization defect). BUT vertices 1 NDC(-1,-1) + 2 NDC(+1,-1) still [DIVERGE] reading [0,0,0,0] cleared — the 1x1 point at an exact NDC corner/edge covers no pixel centre, so it never rasterizes and covered_texel_near's 3x3 finds only cleared texels. NOT a recompiler bug (PS exact + VS v0 exact prove the recompile). Remaining: corner/edge point-readback. Candidate fixes to investigate: write gl_PointSize (larger point so a corner still covers a centre), or nudge readback to scan a wider/whole-fb region for the vertex's non-cleared texel, or render small quads instead of points. LOW — recompiler already GPU-confirmed; this only makes the VS tier-b readback exact for edge vertices.
