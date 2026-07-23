---
id: TASK-133
title: >-
  gpu: Vulkan portability — clamp requested API version +
  portability-enumeration flags (MoltenVK)
status: Done
assignee: []
created_date: '2026-07-16 06:48'
updated_date: '2026-07-16 07:30'
labels:
  - from-audit
  - arch
  - gpu
dependencies: []
ordinal: 139000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable architecture review — portability bug threatening the mac/Bloodborne north-star. crates/gpu/src/vulkan.rs:224 requests vk::API_VERSION_1_3 while the whole recompile/validation path targets a Vulkan 1.1 portability floor — a doc/code contradiction; on older MoltenVK/loader stacks the device is only ~1.2. Also NO VK_KHR_portability_enumeration instance flag / VK_KHR_portability_subset device extension handling, which newer loaders REQUIRE to even enumerate a MoltenVK device. Fix: clamp the requested API version to min(driver, 1.1-or-1.2); add portability-enumeration instance flag + portability-subset device ext when present. Tiny change, very hard to debug later. Keep spirv-val at the 1.1 feature floor as a mandatory CI gate (SPIR-V never exceeds it).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 requested API version clamped to the portability floor, not hardcoded 1.3
- [x] #2 VK_KHR_portability_enumeration flag + portability_subset device ext handled when present (MoltenVK enumerates)
- [x] #3 CI keeps spirv-val gating recompiled SPIR-V at the 1.1 feature floor
<!-- AC:END -->
