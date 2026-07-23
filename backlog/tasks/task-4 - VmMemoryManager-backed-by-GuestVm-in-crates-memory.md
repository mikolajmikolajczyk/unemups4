---
id: TASK-4
title: VmMemoryManager backed by GuestVm in crates/memory
status: Done
assignee: []
created_date: '2026-07-09 15:05'
updated_date: '2026-07-09 17:47'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-3
priority: high
ordinal: 4000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
New crates/memory/src/vm_backend.rs implementing the existing VirtualMemoryManager trait over Arc<GuestVm>, identity semantics. Port VMA BTreeMap + find_free_region + heap cursor verbatim from linux.rs. map = collision check + VMA insert (arena already host-mapped, no mmap); unmap = VMA remove + madvise(MADV_DONTNEED) via identity ptr; protect = tracking-only (same effective behavior as today); get_host_ptr(addr) = addr as *mut u8 guarded by span. Route write_bytes/read_bytes through GuestVm::write_bytes/read_bytes so x86jit SMC/code-page dirty tracking sees loader and handler writes. Reject maps below 0x10000 or above span.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 cargo test -p ps4-memory: map/write/read/zero/unmap round-trips pass
- [x] #2 identity verified: write at 0x400000 then *(0x400000 as *const u8) matches
- [x] #3 collision and out-of-span maps return errors
- [x] #4 workspace builds
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
New crates/memory/src/vm_backend.rs: VmMemoryManager over Arc<GuestVm> impl VirtualMemoryManager. Add ps4-cpu dep (no cycle: cpu !-> memory). Port VMA BTreeMap + find_free_region + heap_cursor(0x4_0000_0000) + is_memory_free from linux.rs verbatim. map = collision-check + VMA insert (no mmap); reject addr<GUEST_BASE(0x10000) or addr+size>span; reserve gadget page 0x30000 as permanent VMA at construction (reject overlaps). Zero-fill fresh maps via GuestVm::write_bytes so reused-after-dirty-unmap regions read zero (arena NORESERVE-mapped once). unmap = VMA remove + madvise(MADV_DONTNEED) on page-aligned covered subrange. protect = tracking-only (no mprotect). get_host_ptr = addr guarded [GUEST_BASE,span). write_bytes/read_bytes route through GuestVm (SMC tracking). Keep LinuxMemoryManager intact; export both from lib.rs. Tests (tests/vm_backend.rs): serialize VM construction w/ Mutex like task-3; map/write/read/zero/unmap round-trips, identity via raw deref, collision+out-of-span+below-base errors, fresh-map-after-dirty-unmap reads zero.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented crates/memory/src/vm_backend.rs (VmMemoryManager over Arc<GuestVm>), exported alongside LinuxMemoryManager (kept fully intact). Tests in crates/memory/tests/vm_backend.rs (7, all pass; VM construction serialized behind a Mutex like task-3). Semantics: map = collision-check + VMA insert + zero-fill via GuestVm::write_bytes (no host mmap); rejects addr<GUEST_BASE(0x10000), addr+size>span, and collisions. unmap = VMA remove + madvise(MADV_DONTNEED) on the page-aligned covered subrange (frees RSS; next touch=zero). protect = tracking-only (no mprotect - arena pre-mapped RWX; matches effective native behavior). get_host_ptr = addr guarded to [GUEST_BASE,span). write_bytes/read_bytes/zero_memory routed through GuestVm (SMC/code-page dirty tracking). Gadget page 0x30000 reserved as a permanent VMA at construction (never zeroed, overlapping maps rejected). Zero-fill on fresh map guarantees MAP_ANONYMOUS parity so a region reused after a dirty unmap reads zero (proven by fresh_map_after_dirty_unmap_reads_zero). ps4-cpu dep added to memory/Cargo.toml - no cycle (ps4-cpu does not depend on ps4-memory). cargo test -p ps4-memory: 7/7. Full workspace cargo test: green (ps4-cpu 4, ps4-memory 7 + others, all doc-tests 0). Release build green (target/release/unemups4 produced). Deferred to task-5: MemoryAccessExt::write/read<T> typed helpers still use the trait's raw get_host_ptr path (bypass SMC) - acceptable for data, matches native; real prot enforcement / guard pages remain future work.
<!-- SECTION:NOTES:END -->
