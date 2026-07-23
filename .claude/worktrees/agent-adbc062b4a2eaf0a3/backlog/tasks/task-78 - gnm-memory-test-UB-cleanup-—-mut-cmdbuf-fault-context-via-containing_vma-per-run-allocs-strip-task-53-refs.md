---
id: TASK-78
title: >-
  gnm/memory: test-UB + cleanup — mut cmdbuf, fault-context via containing_vma,
  per-run allocs, strip task-53 refs
status: Done
assignee: []
created_date: '2026-07-12 07:18'
updated_date: '2026-07-12 07:48'
labels:
  - gpu
  - gnm
  - core
  - chore
dependencies:
  - TASK-75
  - TASK-76
  - TASK-77
priority: low
ordinal: 77000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review findings #4 + #8 + #9 + #10 (cleanup; RUN LAST after task-75/76/77 merge — touches their files). #4 (UB, crates/libs/src/libscegnmdriver/shader_bind.rs tests ~273,316,392): 'let cmdbuf = [0u32; N]' (immutable) is then written through 'cmdbuf.as_ptr() as u64' → IdentityMem.write_bytes → copy through addr as *mut u8. Writing through a pointer derived from a SHARED reference is UB; the tests pass by accident (line 366 already uses 'let mut'). FIX: 'let mut cmdbuf' at every write-through site. #8 (conventions, crates/memory/tests/parse_sb_fault_safety.rs:9,83 + crates/memory/tests/vm_backend.rs:192): 'task-53' refs in source comments violate conventions.md:20 ('Don't reference the current task/fix/PR — belongs in the commit message'). Reword to state the seam/invariant without the ticket number (task-74 just swept these; regressed). #9 (crates/memory/src/vm_backend.rs:330): describe_fault_context does a linear O(n) allocations.values().find(contains) where the new containing_vma does the same lookup in O(log n) — call containing_vma. #10 (crates/gnm/src/pm4/emit.rs:68): RegRun.values is Vec<u32> → 5-8 tiny heap allocs per shader-bind; use a fixed array / SmallVec / inline the header+offset+data writes to kill the churn. All behavior-preserving.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 every test cmdbuf that is written through a raw pointer is declared 'let mut' (no UB); tests still pass (also under a Miri run if available)
- [ ] #2 no 'task-NN' references remain in the memory test comments (git grep clean); rationale preserved reworded
- [ ] #3 describe_fault_context uses containing_vma (no duplicate linear scan)
- [ ] #4 per-run allocations in emit_shader_set eliminated (fixed array/inline); emitted layout unchanged; build/test/clippy/fmt green
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (chore/task-78 @ 833d253, merged). #4 UB: 5 test cmdbuf arrays → let mut + as_mut_ptr() (were immutable [0u32;N] written through as_ptr as *mut = UB). Miri unavailable; as_mut_ptr gives write path mutable provenance. #8: 9 task-NN refs stripped across 6 files (task-53×3/48/216×3/37/36/38/58/65) — git grep clean. #9: describe_fault_context → containing_vma (O(n)→O(log n)). #10: RegRun.values Vec<u32> → data:[u32;2]+len (run1/run2 helpers), per-run allocs eliminated; emitted bytes identical. Verify: 186 tests pass, clippy 0, fmt clean, gnm Vulkan-free, grep clean. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
