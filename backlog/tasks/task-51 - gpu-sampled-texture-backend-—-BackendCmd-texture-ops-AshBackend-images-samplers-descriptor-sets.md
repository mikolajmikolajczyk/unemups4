---
id: TASK-51
title: >-
  gpu: sampled-texture backend — BackendCmd texture ops + AshBackend
  images/samplers/descriptor sets
status: Done
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-12 22:52'
labels:
  - gpu
dependencies:
  - TASK-50
  - TASK-52
priority: medium
ordinal: 50000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Grow BackendCmd (becomes non-Copy) + AshBackend with: create/upload sampled image (from detiled linear bytes), create sampler (S#-derived later; fixed defaults now), bind texture+sampler to pipeline descriptor set. All Vulkan verbs inside AshBackend; executor/cache emit data. Portability: standard sampled-image path only (MoltenVK-safe). Does NOT wire T# decode (P4-20); verified via temp maintainer-run path or P4-06 harness drawing a textured quad from a hardcoded image.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: channel/command-list serialization units for new variants; gnm compiles Vulkan-free against them
- [x] #2 live GPU (maintainer): checkerboard texture through the new path renders on a quad (harness), LD_LIBRARY_PATH=/usr/lib
- [x] #3 existing embedded-draw + present regress-free (Tier A/B)
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add BackendCmd sampled-image/sampler/bind-texture variants (Vulkan-free, Arc<[u8]> pixels) + AshBackend image/view/sampler/combined-image-sampler descriptor path; headless tex_quad harness renders hardcoded checkerboard quad to PNG (AC#2); serialization + descriptor-layout unit tests (AC#1); regress pm4/triangle/examples (AC#3).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented (UNCOMMITTED, worktree feat/task-51). BackendCmd: added CreateImage/UploadImage(Arc<[u8]>)/CreateSampler/BindTexture + texture:Option<TextureBinding> on CreatePipeline; core structs TextureFormat/SamplerFilter/SamplerAddressMode/SamplerDesc/TextureBinding (Vulkan-free). AshBackend: images/samplers maps, create_sampled_image/upload_image(staging+layout transitions)/create_sampler in vulkan.rs; combined-image-sampler descriptor added to set-0 layout in create_host_pipeline; record_draw_list writes it. Harness bin ps4-gpu tex_quad renders hardcoded checkerboard on textured quad offscreen -> PNG. AC1 done (core serialization units + gpu format/filter/address map tests, hand-reasoned). AC2 verified via PNG /tmp/tex51.png (visible red/blue checker). AC3 no-regress: run_examples 6/6; pm4 R/G /tmp/pm4_rg.png; triangle /tmp/gcn_triangle.png. Gate: build 0 err, clippy 0 (non-sdk), fmt clean, tests core 13 / gnm 149 / gpu 4+3+1. NOT committed.
<!-- SECTION:NOTES:END -->
