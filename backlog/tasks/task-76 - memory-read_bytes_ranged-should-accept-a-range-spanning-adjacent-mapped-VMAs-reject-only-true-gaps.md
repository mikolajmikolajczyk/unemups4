---
id: TASK-76
title: >-
  memory: read_bytes_ranged should accept a range spanning adjacent mapped VMAs,
  reject only true gaps
status: Done
assignee: []
created_date: '2026-07-12 07:18'
updated_date: '2026-07-12 07:48'
labels:
  - gpu
  - core
dependencies: []
priority: medium
ordinal: 75000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding #2. VmMemoryManager::read_bytes_ranged (crates/memory/src/vm_backend.rs:393-411) finds the SINGLE VMA containing addr via containing_vma, then rejects if end > vma.end. This is STRICTER than the guest memory contract: a guest read legitimately spanning two ADJACENT, both-mapped VMAs (e.g. a code segment + read-only data placed back-to-back by two mmap calls) is rejected with MemoryFault even though every byte is mapped and readable. Consequence at task-53: parse_sb driven through shader_read_view fails at the first cross-VMA-boundary read → a valid shader is silently skipped. FIX: validate that the WHOLE [addr, addr+size) range is backed by mapped VMAs (walk contiguous adjacent VMAs from addr; reject only if a byte falls in an unmapped GAP or past the arena), not that it fits in one VMA. Keep the over-read protection (finding #1's real goal): a read into an unmapped gap or past the last mapping still returns Err. Coordinate the read_bytes_ranged contract with task-75 (the trait default).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 a read spanning two adjacent, both-mapped VMAs succeeds (returns the bytes) — unit test on the real VmMemoryManager
- [ ] #2 a read that crosses into an unmapped gap (or past the arena/last mapping) still returns Err — unit test (the over-read protection is preserved)
- [ ] #3 a zero-length read and an addr exactly at a VMA end are handled without panic
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-76 @<prior-history>, merged). read_bytes_ranged (vm_backend.rs) now WALKS contiguous adjacent mapped VMAs: covered=addr; while covered<end, containing_vma(covered) (contains guarantees vma.end>covered so covered advances to vma.end); next iter probes prev region's end — non-contiguous/unmapped → None → Err. Zero-len → Ok(vec![]) before lookup; checked_add preserved. Gaps/arena-end still rejected (probe on unmapped addr → None → Err) — over-read protection intact. Composes with task-75: a regs struct spanning two adjacent VMAs now succeeds AND stays bounded. 3 tests (spans_adjacent, rejects_gap, zero_len_and_addr_at_vma_end) + pre-existing straddling test still passes. Verify: ps4-memory 15 pass, clippy 0, fmt clean. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
