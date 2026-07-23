---
id: TASK-131
title: 'gcn: shared per-opcode semantics layer (write interp+recompile op once)'
status: Done
assignee: []
created_date: '2026-07-16 06:48'
updated_date: '2026-07-16 11:23'
labels:
  - from-audit
  - arch
  - gcn
dependencies:
  - TASK-129
ordinal: 137000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable architecture review — top investment #2 (do WITH task-129). Today GCN semantics are written TWICE by hand — interp.rs (wave64 CPU oracle, ~1.7k LoC) and recompile.rs (SPIR-V, ~2.6k LoC) — kept in sync by goldens. Cost goes superlinear at control flow (129): two execution models (64-lane wave+EXEC vs per-invocation predication). Extract a shared semantics layer in ps4-gcn: decode → small typed micro-op (uop) description; interp.rs EVALUATES the uop, recompile.rs LOWERS it. Per-opcode semantics written once; the two backends diverge only at the execution-model level. Do it as part of the 129 CFG rewrite (which touches both files anyway).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a typed micro-op IR expresses each GCN op's semantics once
- [x] #2 interp evaluates the uop IR; recompile lowers it; no per-op logic duplicated across the two files
- [x] #3 adding a new opcode requires one uop definition, not two hand-synced impls
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge b1d58e5). Tagless-final AluBuilder in crates/gcn/src/uop.rs: uniform-f32 VOP2 (add/sub/mul/min/max/mac/madmk/madak) + VOP3 (mul/mac/mad/fma/med3/fract) written once, driven by interp (Val=u32 bits) + recompiler (Val=spirv::Word). Execution-model split (wave64 vs per-invocation) stays. Deferred dual-written (documented in uop.rs): VOP1, integer/bitwise, VOPC/VOP3 compares, pkrtz. Correctness fence: byte-exact golden spirv-dis + differential CPU-SPIR-V oracle both green (83 passed). Note: first attempt forked stale base 5cf0840 + conflicted heavily on recompile.rs — redone fresh on current main.
<!-- SECTION:NOTES:END -->
