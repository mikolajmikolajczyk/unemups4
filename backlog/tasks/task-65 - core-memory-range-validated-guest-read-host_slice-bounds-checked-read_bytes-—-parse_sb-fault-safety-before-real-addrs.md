---
id: TASK-65
title: >-
  core/memory: range-validated guest read (host_slice/bounds-checked read_bytes)
  — parse_sb fault-safety before real addrs
status: Done
assignee: []
created_date: '2026-07-11 18:16'
updated_date: '2026-07-12 06:51'
labels:
  - gpu
  - core
  - gnm
dependencies: []
priority: high
ordinal: 64000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable Runda-1 review BLOCKER. parse_sb's rejection design assumes read_bytes faults at end-of-mapping, but IdentityMem::get_host_ptr (crates/gnm/src/idmem.rs:48-54) returns Some for EVERY non-zero addr, and the default VirtualMemoryManager::read_bytes (crates/core/src/memory.rs:99-109) validates only the START addr then copies size bytes — a read straddling a VMA end over-reads into raw host memory. When task-53 feeds a real register-derived (garbage/encrypted) PGM_LO/HI addr into parse_sb, scan_for_magic walks up to 1 MiB of raw host memory in 4 KiB copy_nonoverlapping windows -> host SIGSEGV, defeating the 'reject encrypted, never decrypt' guarantee. task-36 tests pass only because test BufMem overrides read_bytes with full range checks no production impl has. FIX: add a range-validated read to VirtualMemoryManager (validate [addr,addr+size) against the VMA set, e.g. host_slice(addr,len)->Option<&[u8]> or a bounds-checked read_bytes override on the real manager); task-53 passes a VMA-aware view (not bare IdentityMem) into parse_sb. Parser itself needs no change. MUST land before task-53 wires real addresses.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 read_bytes/host_slice over a range straddling a VMA boundary returns Err/None (no over-read), unit-tested against the real VMM not just BufMem
- [ ] #2 parse_sb over a bogus addr pointing near an unmapped page rejects cleanly (MemoryFault), no SIGSEGV — regression test
- [ ] #3 task-53's parse_sb call site uses the VMA-aware view; bare IdentityMem is not passed to the parser
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-65 @<prior-history>, merged). API: VirtualMemoryManager::read_bytes_ranged(addr,size)->Result (core/memory.rs) — default delegates to read_bytes (doc: VMA backends MUST override). VmMemoryManager::shader_read_view()->VmaBoundedView (memory/vm_backend.rs, re-exported) = the seam task-53 passes into parse_sb; its read_bytes IS the validated read. Real validation: read_bytes_ranged finds containing VMA (allocations.range(..=addr).next_back() + contains), rejects if end>vma.end — whole [addr,addr+size) must sit in one contiguous mapped region (arena is host-mapped once so plain read_bytes over-reads into raw host mem past a region end; VMA check is the only guard). Tests: AC#1 memory/tests/vm_backend.rs (real VmMemoryManager: plain read_bytes over-reads = the vuln, ranged/view reject straddling+unmapped-gap); AC#2 memory/tests/parse_sb_fault_safety.rs (parse_sb over unmapped gap / near VMA end rejects clean, no SIGSEGV; ps4-gnm dev-dep only, no cycle). Verify: workspace 177 pass, clippy 0, fmt clean, gnm Vulkan-free. FLAG: task-53 must pass mgr.shader_read_view() into parse_sb, NOT bare IdentityMem. Combined main gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
