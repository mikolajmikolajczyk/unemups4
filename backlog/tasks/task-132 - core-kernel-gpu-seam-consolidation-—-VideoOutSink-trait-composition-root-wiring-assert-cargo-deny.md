---
id: TASK-132
title: >-
  core/kernel/gpu: seam consolidation — VideoOutSink trait, composition-root
  wiring assert, cargo-deny
status: Done
assignee: []
created_date: '2026-07-16 06:48'
updated_date: '2026-07-16 08:54'
labels:
  - from-audit
  - arch
  - core
dependencies: []
ordinal: 138000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable architecture review — top investment #5 (cheapest, prevents seam erosion as HLE grows 10x for Bloodborne). Three related seam-hygiene fixes: (1) ps4-kernel holds a concrete Arc<ps4_gpu::GpuManager> (process.rs) and calls video_out_register_buffers/submit_flip directly (bridge.rs) — the ONE remaining kernel->Vulkan edge, violating the seam philosophy that put PresentSink/DisplayBufferSource in core; it also hardcodes 1920x1080 + ignores the attr struct (bridge.rs:283). Add a VideoOutSink trait in ps4-core, route the two GPU calls through it, delete the kernel->gpu Cargo edge. (2) Seven process-global Registered<> seams exist; misregistration is runtime-silent (bounded_read.rs already needed a warn-once). Make app/unemups4 the explicit composition root: one wire_host_services() that registers ALL seams and asserts all wired before guest threads start (silent -> boot failure). (3) Enforce layer rules with cargo-deny bans (gnm !ash, kernel !gpu) — today convention-only.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 VideoOutSink trait in core; KernelBridge routes VideoOut through it; kernel->ps4-gpu Cargo edge removed
- [x] #2 app/unemups4 wire_host_services() registers all seams + boot-time assert all wired
- [x] #3 cargo-deny (or equivalent) enforces gnm!ash + kernel!gpu layer bans in CI
<!-- AC:END -->
