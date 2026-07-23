---
id: TASK-59
title: >-
  refactor: route guest-memory typed access through MemoryAccessExt (add
  read_array)
status: Done
assignee: []
created_date: '2026-07-11 14:47'
updated_date: '2026-07-11 15:12'
labels:
  - core
dependencies: []
priority: high
ordinal: 58000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Three ad-hoc unsafe guest-memory readers/writers re-encode 'cast u64 guest addr → *const/*mut T → loop .add(i).read_unaligned()', bypassing the existing MemoryAccessExt (crates/core/src/memory.rs:123-151, read<T>/write<T>) + IdentityMem (crates/gnm/src/idmem.rs). Each owns its own null/zero-length guard + unaligned contract → a fix to one won't propagate, and phase-4 GCN will add more readers. Add read_array<T: Copy>(addr,count)->Vec<T> to MemoryAccessExt and route onto it: read_u64_array/read_u32_array (crates/libs/src/libscegnmdriver/mod.rs:23-44), decode_guest (crates/gnm/src/pm4/decode.rs:228-237), write_label (crates/gnm/src/exec.rs:208-213, via write<u64>). Pre-phase-4 (do before GCN adds more). NON-GOAL: bounds-check vs a real VMM (identity-map still holds); keep the write_eop/eos body-layout split (correct as-is).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 read_array<T: Copy> added to MemoryAccessExt with the null/zero-length guard + unaligned semantics, unit-tested
- [x] #2 libscegnmdriver read_u64/u32_array + pm4 decode_guest + exec write_label route through the trait (no remaining ad-hoc guest *const/*mut T loops in those sites)
- [x] #3 all existing tests + 6-example oracle green; behavior byte-identical; ps4-gnm stays Vulkan-free
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Add read_array<T: Copy>(addr,count)->Vec<T> to MemoryAccessExt (crates/core/src/memory.rs): null/zero-count guard returns Vec::new(); else translate via get_host_ptr, unaligned .add(i).read_unaligned() loop, matching ad-hoc semantics. Unit-test (null, empty, u32, u64, high-addr).
2. libscegnmdriver read_u64_array/read_u32_array (mod.rs): route through IdentityMem::read_array (identity-map, guest ptr==host ptr). Keep count:u32 signature.
3. pm4 decode_guest (decode.rs): keep ptr==0||size<4 guard + word_count=size/4; read words via IdentityMem::read_array::<u32>.
4. exec write_label (exec.rs): route through MemoryAccessExt::write<u64> on IdentityMem; keep addr==0 guard. Keep write_eop/eos body-layout split.
Verify: build/test/clippy/fmt/run_examples 6/6/ps4-gnm Vulkan-free.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-11. Done: Added read_array<T: Copy>(addr,count:usize)->Result<Vec<T>,&'static str> to MemoryAccessExt (crates/core/src/memory.rs) — null/zero-count guard returns Ok(empty); else get_host_ptr + .add(i).read_unaligned() loop (byte-identical to the ad-hoc versions). Unit-tested (null/zero-count, u32+u64 runs incl. >4GB, unaligned offset). Routed all 3 sites through the trait: libscegnmdriver read_u64_array/read_u32_array -> IdentityMem.read_array (dropped their unsafe; record_submit no longer wraps in unsafe); pm4 decode_guest -> IdentityMem.read_array::<u32>; exec write_label -> IdentityMem.write::<u64> (aligned store; label addrs are qword-aligned). write_eop/eos body-layout split kept as-is. No remaining ad-hoc *const/*mut T loops in those 3 sites. Verify: build clean, 115 passed/3 ignored, clippy -D warnings clean, fmt clean, run_examples 6/6 OK, ps4-gnm Vulkan-free. Behavior byte-identical. Blocker: none. Left uncommitted for maintainer review.
<!-- SECTION:NOTES:END -->
