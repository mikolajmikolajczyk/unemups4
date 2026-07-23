---
id: TASK-176
title: >-
  gpu: offscreen RT created R8G8B8A8_UNORM but render pass uses R8G8B8A8_SRGB
  (gamma/VUID mismatch)
status: To Do
assignee: []
created_date: '2026-07-18 18:29'
labels:
  - gpu
  - render-target
  - celeste
dependencies: []
priority: low
ordinal: 180000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found during task-175 R/B investigation (not the R/B cause). Offscreen render targets are created as R8G8B8A8_UNORM (crates/gpu/src/backend.rs:1481) but create_rt_target's render pass declares R8G8B8A8_SRGB (backend.rs:2980). Format mismatch between image and render pass = a Vulkan VUID violation + a gamma (linear vs sRGB) error on offscreen RT content. Pick one consistently (match the guest CB_COLOR's sRGB-ness) so RT color is gamma-correct and the render pass is valid. Low-priority latent bug; surfaces on the RT-composited scenes (title), which we don't reach yet.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Offscreen RT image format and its render-pass format agree (no VUID mismatch)
- [ ] #2 RT gamma matches the guest CB_COLOR sRGB flag
<!-- AC:END -->
