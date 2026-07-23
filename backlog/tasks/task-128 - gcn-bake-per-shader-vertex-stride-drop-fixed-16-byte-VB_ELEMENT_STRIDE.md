---
id: TASK-128
title: 'gcn: bake per-shader vertex stride (drop fixed 16-byte VB_ELEMENT_STRIDE)'
status: Done
assignee: []
created_date: '2026-07-16 06:25'
updated_date: '2026-07-16 17:28'
labels:
  - from-audit
  - gcn
dependencies: []
ordinal: 134000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Hardcode audit (game#2 risk, Tier-1 SILENT WRONG OUTPUT): the recompiler bakes VB_ELEMENT_STRIDE=16 into the SPIR-V OpTypeRuntimeArray ArrayStride (recompile.rs:314). The interp reads the true stride from the V# (word1[29:16]) but the recompiler resolves the descriptor symbolically and never sees the bytes, so it cannot bake a per-shader stride. exec.rs:598 DEFERS a draw whose bound V# stride != 16. Celeste's vertices are exactly 16 B (one vec4). A game with 12/24/32-B vertex records either defers ALL draws (renders nothing) or, if the stride ever mismatches an already-baked 16, reads every vertex past #0 at the wrong offset (silent scrambled geometry). Fix: make the recompiler emit the actual per-pairing stride (bake it when the provider knows the bound V#, or make the SSBO stride a spec-constant/push-constant supplied at bind).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 recompiled VS uses the bound V#'s real element stride, not a fixed 16
- [ ] #2 a draw with a non-16-byte vertex V# renders correct geometry (corpus coverage), not defer
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Recompiler+oracle level CLOSED by task-130 slice 5 (merge 9df9edb): stride is a SpecId-0 OpSpecConstant, non-16 no longer defers, task-122 oracle proves the fetch at stride 24. GPU-level NOT yet closed -> task-140: backend must specialize the stride (VkSpecializationInfo) or switch to a push constant, and resolve the spec-const-at-create vs stride-out-of-key tension. Keep this task open until 140 lands + a non-16 stride renders correctly on real GPU.
<!-- SECTION:NOTES:END -->
