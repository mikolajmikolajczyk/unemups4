---
id: TASK-57
title: >-
  test: GCN decoder cross-validation vs shadPS4 disassembly (Tier-3 external
  oracle)
status: To Do
assignee: []
created_date: '2026-07-11 13:54'
updated_date: '2026-07-11 13:54'
labels:
  - test
dependencies:
  - TASK-38
priority: medium
ordinal: 56000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Our differential oracle (interpreter vs recompiler, task-39/41) proves INTERNAL consistency, not EXTERNAL correctness — a shared misread of a GCN encoding passes both, and our self-authored golden 'known answers' could themselves be wrong. Add an external ground-truth check: compare our task-38 GCN decoder's disassembly of the synthetic corpus (task-37) against shadPS4's decode of the same bytes (shadPS4 = a mature, proven GCN→SPIR-V PS4 emulator). shadPS4 checked out/built locally and gitignored (like data/oo_sdk); either drive its decoder over our corpus blobs via a small harness / vendored decode tables, or diff against captured shadPS4-disassembly fixtures committed as golden. Catches decode bugs our own goldens cannot. Tier 2 (full reference-SPIR-V from shadPS4's translator) is explicitly DEFERRED. NON-GOAL: running shadPS4's recompiler.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Our decoder's disassembly of every corpus shader matches shadPS4's decode per-instruction (committed golden fixture, or a harness over a local shadPS4 checkout)
- [ ] #2 A divergence (opcode/operand mismatch) fails the test with a clear per-instruction diff
- [ ] #3 shadPS4 source is gitignored / not committed; the check skips cleanly when absent (like the OO SDK), never breaks CI
<!-- AC:END -->
