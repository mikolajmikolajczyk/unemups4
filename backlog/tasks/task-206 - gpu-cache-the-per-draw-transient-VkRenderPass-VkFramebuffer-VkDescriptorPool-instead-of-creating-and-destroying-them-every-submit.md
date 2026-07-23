---
id: TASK-206
title: >-
  gpu: cache the per-draw transient VkRenderPass / VkFramebuffer /
  VkDescriptorPool instead of creating and destroying them every submit
status: To Do
assignee: []
created_date: '2026-07-21 18:28'
labels:
  - gpu
  - perf
  - vulkan
dependencies:
  - TASK-203
priority: medium
ordinal: 211000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
record_passes builds transient Vulkan objects per PASS and destroys them all after the fence wait: crates/gpu/src/backend.rs:1236-1237 collect transient_rp / transient_fb, :1267 collects desc_pools, and :1466-1474 destroy the lot. Each pass creates a fresh VkRenderPass + VkFramebuffer (create_rt_target / create_videoout_load_target) and record_pass_into creates a whole VkDescriptorPool with max_sets: 1 just to allocate a single descriptor set (backend.rs:3122-3138).

Celeste records on the order of 29 draws per frame, so that is roughly 87 driver-side object create/destroy pairs per frame. These are exactly the objects Vulkan expects to be created once and reused: render passes are keyed by a small set of (format, load-op, initial/final layout) combinations, framebuffers by (render pass, image view, extent), and descriptor sets should come from a pool that is RESET per frame rather than destroyed.

Cache all three, keyed on their real parameters, invalidated when the underlying render target is recreated. Note the codebase already owns two comparable caches to mirror rather than invent: ResourceCache and shader/pipeline_cache.rs.

Depends on task-203 — the instrumentation must first show what share of the frame this churn actually costs, so the work is justified by a measurement instead of by the object count looking alarming.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 render passes and framebuffers are cached and reused across submits, keyed on their real parameters, invalidated when the render target is recreated
- [ ] #2 descriptor sets come from a per-frame reset pool rather than a freshly created and destroyed VkDescriptorPool per draw
- [ ] #3 measured before/after per-frame improvement recorded in the notes, using the task-203 counters
- [ ] #4 no validation errors under a validation-layer run; scene renders unchanged; build + clippy clean, cargo test --workspace green
<!-- AC:END -->
