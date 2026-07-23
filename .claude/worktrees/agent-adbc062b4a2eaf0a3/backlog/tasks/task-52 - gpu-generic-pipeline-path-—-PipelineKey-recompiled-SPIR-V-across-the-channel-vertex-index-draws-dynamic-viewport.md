---
id: TASK-52
title: >-
  gpu: generic pipeline path — PipelineKey + recompiled SPIR-V across the
  channel, vertex/index draws, dynamic viewport
status: Done
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-12 17:14'
labels:
  - gpu
dependencies:
  - TASK-40
  - TASK-42
  - TASK-46
priority: medium
ordinal: 51000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Extend BackendCmd/AshBackend from hardcoded embedded pipeline to arbitrary: BindPipeline{key:PipelineKey, vs/ps:Arc<[u32]> SPIR-V + I/O layout} (SPIR-V crosses channel only on cache miss; backend caches PipelineKey→vk::Pipeline), vertex-buffer binding, DrawIndexed/index-buffer commands, dynamic viewport/scissor, render pass over a described target (videoout image first, P4-11). Pipeline creation stays portable subset. Embedded pipelines migrate onto the generic path (delete BindEmbeddedPipeline special-case) or stay chained — task decides, favor one path. Does NOT add MRT/depth beyond P4-11.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: mock-replay asserts command-list ordering/shape; pipeline cache keyed correctly (same key → no 2nd create, counter)
- [x] #2 live GPU: embedded R/G draw still renders through generic path (ps4-pm4-test Tier B)
- [ ] #3 live GPU: corpus VS+PS + hardcoded vertex buffer renders a triangle via hand-fed list
- [x] #4 no ash::vk outside crates/gpu
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add guest-minted PipelineId + BackendCmd::{CreatePipeline{id,vs_spirv,ps_spirv,PipelineKey,TargetDesc},BindPipeline{id}} to ps4-core; guest-side PipelineCache (PipelineKey->PipelineId get-or-mint, create-count hook) in gnm; migrate embedded R/G onto generic path (delete BindEmbeddedPipeline); accumulate one BackendCmd list per submit; display-side PipelineId->vk::Pipeline cache in AshBackend building from SPIR-V, per-frame fence not device_wait_idle; add hand-fed-list harness; headless replay test asserts ordering+cache counter.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC#2 CONFIRMED live 2026-07-12 (maintainer): ps4-pm4-test Tier B — embedded VS id=0 (fullscreen triangle) + PS id=1 renders the R/G gradient full-window through the generic CreatePipeline/BindPipeline path; gradient (not solid) proves the PS executed. exit(0), no vk validation error. Embedded migration off BindEmbeddedPipeline verified live, no regression.
<!-- SECTION:NOTES:END -->

## Notes

Landed: generic guest-minted PipelineId path (decision-7) — PipelineId in ps4_core::gpu, BackendCmd::CreatePipeline{id, vs/ps: Arc<[u32]>, PipelineKey/TargetDesc} + BindPipeline{id}, BindEmbeddedPipeline DELETED (embedded R/G fully migrated onto generic path). Guest-side PipelineCache (PipelineKey->PipelineId get-or-mint, driver-owned, persists across submits, created_count() test hook). Display-side id->vk::Pipeline map in AshBackend. ONE command list PER SUBMIT: executor accumulates Vec<BackendCmd>, ships one RunCommandList; backend records one CB + per-list fence wait (no per-draw device_wait_idle).

AC status: #1 (headless replay + cache counter) VERIFIED. #4 (no vk outside crates/gpu) VERIFIED. #2 (live: embedded R/G through generic path, ps4-pm4-test Tier B) PENDING maintainer live-GPU run — foundation ready, headless can't exercise the vk pipeline. #3 (live: hardcoded-vertex-buffer triangle) MOVED TO task-53: no BindVertexBuffer/DrawIndexed/vertex-input-state was added (scope reserved for task-53 cache-fed buffers); handfed_list.rs renders a gl_VertexIndex triangle only. Dynamic viewport also deferred to task-53 (pipeline currently bakes static viewport/scissor).

Two reviews (core/gnm + gpu-backend) CLEAN, zero criticals: per-submit fence refactor sound (in-line wait before CB reset/free), cache keying full PipelineKey (no collision), tests genuinely independent, portable subset preserved. Deferred (documented): per-submit GCN provider rebuild + cache-hit still recompiles+discards SPIR-V -> task-53 driver-owned provider.
