---
id: TASK-98
title: >-
  gcn/gnm: tiled-texture correctness — interp oracle detile + macro-tiling +
  full S# filter
status: Done
assignee: []
created_date: '2026-07-12 23:51'
updated_date: '2026-07-13 21:26'
labels:
  - gpu
  - gnm
  - gcn
dependencies:
  - TASK-55
  - TASK-50
priority: medium
ordinal: 97000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-55 review MINORs (both tiling-related, scoped out of the linear corpus AC path). (M2) The interpreter oracle sample_texture/texel (interp.rs ~1136) reads the texture as RAW-LINEAR — tiling ignored — while the gnm GPU path (cache/mod.rs emit_image_upload) DETILES a Thin1d surface before upload. For any tiling_index != 0 texture the two sample different texels, so recompiler==interp==GPU would NOT close for a tiled texture (harmless today: the corpus texture is linear, detile=identity). Reconcile: the oracle must detile the same way the upload does (or both agree) before a tiled texture corpus lands. Also: task-55 fixed the tiling_index decode mask to 5 bits [24:20] (was 0x7), but the DOWNSTREAM tiling_index->detile mapping still only handles linear + Thin1d — a macro-tiled index (>=8, GFX 2D) currently has no correct detile (task-50 deferred 2D macro-tiling); decide defer-vs-implement when a tiled corpus needs it. Minor: S# filter is a 2-bit field word2[21:20] (POINT=0/BILINEAR=1) but only bit 20 is read — fine for the point/bilinear subset, revisit for more sampler modes. Ref: task-50 (detile), task-55.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 the interpreter oracle detiles a tiled texture identically to the gnm upload path, so recompiler==interp==GPU closes for a 1D-thin (and, when implemented, macro-tiled) texture — differential test
- [x] #2 a macro-tiled (tiling_index>=8) texture either detiles correctly or defers cleanly (no silent mis-detile)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Root cause: oracle (interp.rs texel) reads texture RAW-linear; upload (cache tile::detile) detiles Thin1d -> the two sample different texels for any tiling_index!=0, so interp==recompiler==GPU can't close for tiled. Also exec.rs maps tiling_index!=0 -> Thin1d (silent mis-detile for macro >=8).

gcn(interp) can't import gnm(tile.rs) [gnm->gcn->core], so single source of truth = new ps4-core::tiling: micro_tile_index/thin1d_texel_offset swizzle + tile_kind(u8)->{Linear,Thin1d,Macro}.
1. ps4-core::tiling module + unit tests.
2. gnm cache/tile.rs: use core swizzle (dedup private copy).
3. gnm exec.rs setup_draw: macro-tiled T# -> clean defer (AC#2). bind_texture: classify via core.
4. gcn interp.rs texel(): Thin1d -> swizzled offset, Macro -> clean InterpError (AC#1/AC#2).
5. Differential test: tiled texture oracle==detile.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-13. DONE (uncommitted, awaiting commit approval).
Root cause: two texture-read paths drifted. GPU upload (gnm cache::tile::detile) un-swizzles Thin1d; interp oracle (interp.rs texel) read RAW-linear -> different texel for any tiling_index!=0, so interp==recompiler==GPU could not close for tiled. Also exec.rs mapped tiling_index!=0 -> Thin1d (silent mis-detile for macro >=8).

Fix (single source of truth; gnm->gcn->core so neither can import the other):
- NEW crates/core/src/tiling.rs: micro_tile_index/thin1d_texel_offset swizzle + tile_kind(u8)->{Linear(0),Thin1d(1..=7),Macro2d(>=8)} + unit tests.
- gnm cache/tile.rs: dropped private swizzle copy, uses core (dedup).
- gnm exec.rs setup_draw: macro-tiled T# -> clean defer before pipeline build (AC#2). bind_texture classifies via core.
- gcn interp.rs texel(): Thin1d -> core swizzled offset (== upload byte), Macro2d -> InterpError::UnsupportedTiling (AC#2). Linear path byte-identical -> no regression to existing linear differential.

AC#1: interp tests image_sample_thin1d_reads_the_swizzled_texel_not_the_linear_one (hand-laid marker@offset20 vs decoy@offset12; proven to FAIL with linear oracle). Structurally oracle+upload share core swizzle.
AC#2: image_sample_macro_tiled_faults_instead_of_mis_detiling + gnm defer.
253/253 workspace tests, clippy -D warnings clean, fmt clean.

Deferred (task note, not AC): S# min-filter bit22 still unread (only mag bit20) — correct for point/bilinear subset where mag==min; revisit for more sampler modes. Threshold 1..=7=Thin1d is first-approx (preserves pre-task upload behavior); real GB_TILE_MODE table nuance deferred until a tiled-corpus/retail needs it.
<!-- SECTION:NOTES:END -->
