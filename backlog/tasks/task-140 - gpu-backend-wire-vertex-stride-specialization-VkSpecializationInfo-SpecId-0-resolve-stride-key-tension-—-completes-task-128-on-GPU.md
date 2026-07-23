---
id: TASK-140
title: >-
  gpu backend: wire vertex-stride specialization (VkSpecializationInfo SpecId 0)
  + resolve stride-key tension — completes task-128 on GPU
status: Done
assignee: []
created_date: '2026-07-16 12:17'
updated_date: '2026-07-16 17:28'
labels:
  - gpu
  - gcn
  - gnm
dependencies:
  - TASK-130
priority: high
ordinal: 146000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-130 slice 5 made the vertex element stride a SpecId-0 OpSpecConstant in the recompiled VS (default 16) and flows the real guest V# stride onto CreatePipeline StorageBinding.stride, but the GPU BACKEND does not yet consume it. Two things remain to actually render a non-16 stride correctly on real hardware:

(1) BACKEND SPECIALIZATION: at vkCreateGraphicsPipelines, build a VkSpecializationInfo mapping SpecId 0 -> StorageBinding.stride and attach it to the VS stage. Until this lands, every pipeline uses the module default (16), so a non-16 stride renders wrong (currently masked: the corpus is all stride-16, and Celeste crashes at the RADV submit — task-139 — before any non-16 geometry). File crates/gpu backend/vulkan.rs pipeline-create path.

(2) RESOLVE THE STRIDE-KEY TENSION (decision needed): Vulkan spec constants bake at pipeline-CREATE, not bind. task-130 slice 6 deliberately keeps stride OUT of PipelineKey (intent: one pipeline serves all strides). Those two are inconsistent — if the backend specializes SpecId 0 at create while stride is not in the key, two draws with the same shader+layout but different stride collide on PipelineKey and reuse the first stride's pipeline (silent wrong). Pick ONE: (a) RECOMMENDED — make stride a PUSH CONSTANT (or UBO field) read by the fetch instead of a spec constant, so one pipeline truly serves all strides dynamically and stride legitimately stays out of the key; (b) keep the spec constant but put stride back INTO PipelineKey (one pipeline per stride value). (a) matches slice-6's 'stride out of key / one pipeline all strides' intent and avoids a pipeline explosion across strides; it requires changing the recompiler fetch from OpSpecConstant to a push-constant load (small, and removes the SpecId-0 constant). Coordinate the recompiler change with the golden + task-122 oracle (both must stay green; the oracle already resolves stride via a binding, so it can resolve a push-constant stride the same way).

VERIFY: a headless/corpus test that renders a non-16 stride (12/24/32) on the real GPU path (or via the pipeline-create specialization/push-constant plumbing) and a PipelineKey test proving no wrong-stride reuse under the chosen option. Closes task-128 fully (GPU-level).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The recompiled VS's vertex stride is supplied at pipeline build (VkSpecializationInfo SpecId 0) OR converted to a push constant/UBO and supplied at bind, so a non-16 guest V# stride renders correctly on the real GPU
- [x] #2 The stride-key tension is resolved: EITHER stride is a push constant and legitimately stays out of PipelineKey (one pipeline all strides), OR stride is a spec constant and is IN the key (one pipeline per stride) — no silent wrong-stride pipeline reuse either way
- [x] #3 golden disasm + task-122 differential oracle + spirv-val stay green through the recompiler change; a non-16-stride render/plumbing test proves correctness
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 56d9ad0). Fixed the non-16-stride REGRESSION by moving stride from the never-specialized OpSpecConstant to a 2nd VS push-constant member (num_records@0, stride@4). One pipeline serves all strides dynamically, stride stays OUT of PipelineKey (tension resolved), OpSpecConstant removed (closes task-141's spec-val-after-specialization concern too). Golden regen + task-122 oracle @stride-24 + spirv-val all green, 317 tests. CLOSES task-128 (GPU-level: non-16 renders correctly). LIVE: Celeste's SSBO draws reaching the fetch are all stride-16, so this is NOT the black-frame cause. NEW LEAD for task-149: the agent saw only ~1 SSBO fetch reach the executor ('stalled behind Graphics:: dlsym misses: Graphics::VertexShader/DrawPrimitives') — i.e. most of Celeste's ~1000 submitted draws DEFER before the fetch. task-149 candidate (a) draws-defer is now the lead: find WHY the draws defer (T#/S#/V#/CB resolve fail, or the Graphics:: managed-shader dlsym path).
<!-- SECTION:NOTES:END -->
