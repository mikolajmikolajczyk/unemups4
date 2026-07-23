---
id: TASK-93
title: 'gnm: handle flipped viewport (negative YSCALE) in derive_viewport'
status: Done
assignee: []
created_date: '2026-07-12 16:16'
updated_date: '2026-07-12 17:11'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-46
priority: low
ordinal: 92000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-46 review finding (MINOR, uncertain). derive_viewport does a plain y=yoffset-yscale, height=2*yscale. GCN commonly programs a Y-flipped viewport (YSCALE=-H/2, YOFFSET=H/2); current math then yields y=H (should be 0 for a top-left origin) and height=-H. A negative Vulkan viewport height IS the intended Y-flip passthrough, but the derived top-left y is wrong for that convention. The task-46 golden only exercises unflipped (YSCALE=+540), so the flip path is untested. Decide the backend viewport convention (this couples with the task-91 diff_harness VS Y-flip readback artifact) and derive y/height consistently; add a flipped-viewport golden. Ref: derive.rs derive_viewport (PA_CL_VPORT_YSCALE/YOFFSET).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 derive_viewport yields a correct top-left origin + height for a flipped viewport (YSCALE<0), consistent with the backend/present convention
- [x] #2 headless golden covers both unflipped and flipped (negative YSCALE) viewports
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Fix derive_viewport Y: top-left y = yoffset - yscale.abs() (correct for both signs); keep height=2*yscale so negative yscale yields negative Vulkan height (Y-flip passthrough). Add flipped+unflipped golden asserting hand-computed y/height literals.
<!-- SECTION:PLAN:END -->
