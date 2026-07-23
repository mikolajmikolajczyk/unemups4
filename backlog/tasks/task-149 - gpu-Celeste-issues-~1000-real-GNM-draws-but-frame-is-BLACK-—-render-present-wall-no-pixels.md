---
id: TASK-149
title: >-
  gpu: Celeste issues ~1000 real GNM draws but frame is BLACK — render/present
  wall (no pixels)
status: Done
assignee: []
created_date: '2026-07-16 16:20'
updated_date: '2026-07-16 20:11'
labels:
  - gpu
  - gnm
  - gcn
  - celeste
  - retail
  - bug
dependencies: []
priority: high
ordinal: 155000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
THE remaining Celeste geometry wall, now cleanly isolated. After task-148 (direct-memory model -> Mono no longer aborts, asset thread survives), Celeste runs sustained gameplay and the guest issues a REAL render workload: ~800-1366 sceGnmDrawIndexAuto/DrawIndexOffset draws, ~157-254 SubmitAndFlip, 986 shader/texture binds per run (VS/PS set with real reg pointers, indexed draws with real vertex counts). Yet the presented frame is uniform BLACK (PNG oracle). So assets load + real geometry IS submitted — the blocker is in OUR GPU render/present path: real draws resolve but produce no visible pixels. This is NOT asset-loading, NOT the mutex/direct-memory aborts (all fixed), NOT a spirv-val/pipeline-layout crash (139/141 fixed).

Investigate the executor + backend for why the ~1000 draws yield black. Candidates to check with RUST_LOG=warn,ps4_gnm=info + the PNG oracle: (a) the draws DEFER (T#/S#/const-buffer/vertex V# resolve fails -> needs_texture/needs_storage/needs_const defer -> no draw recorded) — count how many of the ~1000 draws actually reach the backend vs defer, and why; (b) Celeste renders to OFFSCREEN render targets (task-56 RT-as-texture) that are never composited into the presented videoout framebuffer — check whether the final SubmitAndFlip/present samples the rendered RT or a blank videoout image; (c) viewport/scissor/depth/cull/blend state leaves geometry off-screen or fully discarded; (d) the draws render but to a target whose format/clear leaves black, or the present blit picks the wrong image. Instrument: per-submit, log draws-recorded-vs-deferred + the target each draw writes + whether present samples an RT. Dump per-RT PNGs (UNEMUPS4_DUMP_RT_PNG if it exists, else add a debug dump) to see if ANY offscreen RT has content even when the final frame is black. Assets at /home/mikolaj/PS4/CUSA11302 gitignored, NEVER commit; PNG oracle for all frame claims (logs lie); RUST_LOG sane (firehose kills runs). Related: task-56 (RT-as-texture + compositor), doc-6 GNM discovery log.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Root-caused: why ~1000 real GNM draws produce a black frame — draws deferring (which resolve fails), or rendering to an uncomposited offscreen RT, or state discarding geometry, or a present/blit picking the wrong image
- [ ] #2 Fix lands so the submitted geometry reaches visible pixels; PNG oracle shows non-black content (the orchestrator Reads the PNG)
- [x] #3 If the cause is offscreen-RT compositing, it ties into task-56 (RT-as-texture) — a live title now drives an offscreen RT, so task-56 AC#3 becomes verifiable
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-07-16: two root-caused bugs FIXED + merged (16a866c). Defer histogram: recorded 422, tex_unresolved=953 (dominant), cb_multi=240, needs_gcn=0. (1) managed-shader Graphics::VertexShader hypothesis DISPROVEN (needs_gcn=0 -> Celeste binds real GNM shaders register-path). (2) FIX texture InlineVSharp: PS T# is InlineVSharp{sgpr:0} (256-bit T#/S# inline in user-SGPRs, no pointer) but derive_texture_binding deref'd it as a SetPointer -> MemoryFault -> all textured draws deferred; now dispatches on binding.source (new derive_texture_inline). tex_unresolved 953->0, recorded 422->879. (3) FIX videoout CLEAR-clobber: up to 5 videoout draws/submit shared a loadOp=CLEAR pass -> each cleared the previous; now CLEAR-first/LOAD-rest. (4) Offscreen-RT NOT the blocker (Celeste renders mostly straight to videoout). AC#2 (non-black) STILL OPEN -> task-152: a magenta clear-probe proved present blits correctly + ZERO fragments survive any draw -> deeper VERTEX/RASTERIZATION wall (clipped positions / degenerate viewport / bad vertex-index fetch / mis-resolved MVP const buffer). 207 tests.
<!-- SECTION:NOTES:END -->
