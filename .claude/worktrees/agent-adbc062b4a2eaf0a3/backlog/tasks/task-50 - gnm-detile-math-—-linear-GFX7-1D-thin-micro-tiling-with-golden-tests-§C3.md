---
id: TASK-50
title: 'gnm: detile math — linear + GFX7 1D-thin micro-tiling with golden tests (§C3)'
status: Done
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-12 15:54'
labels:
  - gpu
  - gnm
dependencies: []
priority: medium
ordinal: 49000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Pure-logic detile(bytes,layout)->linear (+ inverse for future readback) for first tile modes: linear-general/linear-aligned + 1D-thin micro-tiled (8×8 micro-tile order) for common 32bpp/64bpp texel sizes; ResLayout grows Texture{dfmt,nfmt,extent,tiling}/RenderTarget{…} carrying tiling+compression fields (compression forced OFF §C9). Ref: freegnm/AddrLib, GPCS4 tiler. 2D macro-tiling DEFERRED (confirmed) — sized when corpus/Bloodborne demands; the seam (tiling enum + detile dispatch) is what this cements. Headless golden tests vs hand-computed tile patterns.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: golden — known 8×8-tiled input → exact expected linear for ≥2 texel sizes + non-tile-aligned extents
- [x] #2 headless: linear layouts are identity (zero-copy-eligible); tiled report zero-copy-impossible
- [x] #3 headless: property test detile∘tile==identity on random surfaces
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add Tiling/Compression + ResLayout::Texture/RenderTarget in gnm; pure detile/tile for linear (identity) + GFX7 1D-thin 8x8 micro-tile (32/64bpp); zero-copy-eligible predicate; headless golden + identity + seeded-PRNG property tests.
<!-- SECTION:PLAN:END -->
