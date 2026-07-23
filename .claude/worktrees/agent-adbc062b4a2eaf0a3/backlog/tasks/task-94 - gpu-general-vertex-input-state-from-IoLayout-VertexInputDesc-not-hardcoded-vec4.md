---
id: TASK-94
title: >-
  gpu: general vertex-input state from IoLayout/VertexInputDesc (not hardcoded
  vec4)
status: Done
assignee: []
created_date: '2026-07-12 18:22'
updated_date: '2026-07-12 18:57'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-53
  - TASK-45
priority: medium
ordinal: 93000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-53 review finding (both backend + exec reviewers, one root cause). create_host_pipeline hardcodes exactly one vertex attribute location=0 binding=0 R32G32B32A32_SFLOAT offset=0 and reads only vertex_layout.stride — discarding the derived per-attribute dfmt/nfmt/dst_sel/offset (VertexAttribute in vbuf.rs, carried via VertexInputDesc) and attribute_count. Fine for the tested single-vec4 corpus (passthrough_vs), WRONG for any other vertex format (Format8_8_8_8 UNORM colors, Format16_16 UVs) or attribute_count>1 or >=2 vertex buffers: the declared vertex-input won't match the SPIR-V input decorations -> wrong fetch or pipeline-creation validation error. FIX: thread VertexInputDesc/PipelineKey.vertex_layout attributes to the display side; map dfmt/nfmt->vk::Format per attribute; declare each binding (stride per buffer) + attribute (location/format/offset). Data is already threaded, just unused on the backend side.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 backend declares vertex bindings+attributes from the derived VertexInputDesc (dfmt/nfmt->vk::Format, attribute_count, per-attribute offset), not a hardcoded vec4
- [x] #2 a >=2-buffer / non-vec4-format corpus draw renders correctly (headless mock asserts the pipeline vertex-input; live when a corpus ELF exists)
- [x] #3 embedded empty-vertex path unchanged
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Extend core VertexLayout to inline per-attr {location,binding,VertexFormat,offset}+per-binding stride (Copy/Hash preserved); populate in gnm from VertexInputDesc; map VertexFormat->vk::Format in backend; declare N bindings+attrs; embedded None path unchanged; hand-reasoned multi-attr AC tests.
<!-- SECTION:PLAN:END -->
