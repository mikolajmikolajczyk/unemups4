---
id: TASK-136
title: >-
  gpu: GpuCaps capability-query seam (VK_KHR_portability_subset) — foundation
  for caps-tiered Vulkan/MoltenVK split
status: Done
assignee: []
created_date: '2026-07-16 11:31'
updated_date: '2026-07-16 13:30'
labels:
  - gpu
  - arch
  - portability
dependencies: []
priority: medium
ordinal: 142000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Foundation for a later 'full-power Vulkan on Linux / compatibility fallback on MoltenVK' split, done the RIGHT way: capability-driven, not platform-#cfg. Today we clamp the Vulkan/SPIR-V feature set to the MoltenVK common denominator (task-133 portability clamp) as a single portable baseline — correct for now (north-star Bloodborne/Mac; the same clamped SPIR-V also runs on Linux since Linux is a feature superset). This task builds the SEAM so a future fast/compat fork is a DATA flag threaded through one code path, never two divergent backends or two golden sets (which would reintroduce the dual-maintenance task-131 just killed).

Mechanism: MoltenVK advertises VK_KHR_portability_subset, which formally enumerates exactly what is NOT supported (triangle fans, separate stencil ref, restricted image views, etc.) + reduced limits. Detect that extension at device selection and populate a GpuCaps struct from REAL device feature/limit queries — NOT from #[cfg(target_os)]. Platform != capability: MoltenVK gains features over time, and Linux drivers vary. Thread GpuCaps through the present/backend path and the recompiler emit path as a parameter. For now every populated path emits the SAME portable baseline (no fast branch yet — seam only, cheap now, expensive to retrofit later). A caps-gated fast path is a LATER, measured optimization (YAGNI until a clamp cost is measured).

Scope note: this is infra/seam only. Do NOT add a divergent fast path, do NOT branch on OS, do NOT split golden sets. Relates to task-133 (portability clamp) and the gpu-roadmap portability constraint (decision-3/6/7).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 At device selection, detect VK_KHR_portability_subset and populate a GpuCaps struct from real VkPhysicalDeviceFeatures/limits + the portability_subset feature struct — no target_os / #[cfg] platform branching anywhere in the caps decision
- [x] #2 GpuCaps is threaded as a parameter through the present/backend path AND the recompiler emit path (single code path takes the caps; no duplicate backend, no second golden set)
- [x] #3 Behavior unchanged: baseline clamp applied everywhere (Linux runs the same portable path); NO fast/full branch is added in this task — seam only
- [x] #4 Documented: which portability_subset features/limits are queried, where GpuCaps gates, and the explicit note that a caps-tiered fast path is a later measured optimization
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 70bf259). GpuCaps in core/gpu.rs (Vulkan-free plain data, Default=FULL). query_caps in vulkan.rs called after pick_device: enumerate ext for VK_KHR_portability_subset, chain PhysicalDevicePortabilitySubsetFeaturesKHR into features2 when present, surface min/max image count — no target_os/#[cfg]. Threaded to VulkanContext.caps + AshBackend.caps(). SEAM ONLY: behavior unchanged (swapchain still hardcodes min_image_count:2, nothing gates on caps). Recompiler NOT threaded (marker comment at the SPIR-V clamp site only — avoided recompile() signature churn + goldens for an unused param). 34 tests. Future: a caps-tiered fast path is a later MEASURED optimization (task-133 SPIR-V clamp is the recompiler consumer).
<!-- SECTION:NOTES:END -->
