---
id: TASK-75
title: >-
  gnm/libs+core: bounds-check the Set*Shader regs read + make read_bytes_ranged
  default safe
status: Done
assignee: []
created_date: '2026-07-12 07:18'
updated_date: '2026-07-12 07:48'
labels:
  - gpu
  - gnm
  - core
dependencies: []
priority: high
ordinal: 74000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review findings #1 + #3 (the HIGH one). #1: read_reg_block (crates/libs/src/libscegnmdriver/shader_bind.rs:65-70) reads VS_STAGE_REG_FIELDS=7 / PS_STAGE_REG_FIELDS=12 dwords via bare IdentityMem.read_array — IdentityMem::get_host_ptr returns Some for EVERY addr, so the read is UNBOUNDED. task-70 widened this from 4 to 7/12 dwords, so a vs_regs/ps_regs pointer near an unmapped page over-reads 28/48 bytes past the mapping → host SIGSEGV or adjacent-host-memory leaked into RSRC/context registers. task-65 landed the fix tool THE SAME ROUND (VmMemoryManager::shader_read_view() / read_bytes_ranged, a VMA-bounded read) but this path still uses raw IdentityMem. FIX: route read_reg_block through a bounds-checked read that consults the VMA set — reach the registered VMA-aware manager (like the registered kernel/dirty_source/present_sink singletons; check how the HLE handler can reach the VmMemoryManager, or register a bounded-read source) rather than the unbounded IdentityMem. IdentityMem is inherently unbounded — do NOT rely on it for untrusted guest pointers. On an out-of-bounds/unmapped regs read, return None → emit nothing (preserve task-69's no-bogus-bind guarantee). #3: the VirtualMemoryManager::read_bytes_ranged DEFAULT (crates/core/src/memory.rs) silently delegates to the unbounded read_bytes — the doc says 'VMA backends MUST override' but nothing enforces it, so any future impl (task-53/55 texture/compute memory) silently inherits the exact over-read task-65 closed. Change the default to NOT be silently unsafe: either remove the default (force every impl to provide range validation) or make the default return Err('ranged read not implemented for this backend') so a missing override fails loudly instead of over-reading. Coordinate the contract with task-76 (VmMemoryManager's override). Keep ps4-gnm Vulkan-free; no crypto.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 read_reg_block over a regs pointer that would over-read past its mapping returns None (no PM4, no bind) instead of SIGSEGV/over-reading — regression test with a regs block near an unmapped boundary
- [ ] #2 the bounded read consults the VMA set (not bare IdentityMem) for the untrusted regs pointer
- [ ] #3 VirtualMemoryManager::read_bytes_ranged no longer silently degrades to unbounded read_bytes: a non-overriding impl either won't compile or returns Err (documented), not an over-read
- [ ] #4 build/test/clippy/fmt green; ps4-gnm Vulkan-free; existing Set*Shader round-trip tests still pass
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (fix/task-75 @ c539626, merged). #1: read_reg_block now reaches a bounded read via NEW ps4_core::bounded_read singleton (crates/core/src/bounded_read.rs) mirroring register_kernel/dirty_source/present_sink: BoundedRead trait + blanket impl for Arc<RwLock<Box<dyn VirtualMemoryManager>>> routing to read_bytes_ranged; registered at boot in main.rs with process.memory.clone() (live VMA set). read_reg_block → bounded_read().read_ranged(regs, fields*4) → unmapped/straddle = Err → None → no PM4/bind. Chosen over threading manager through syscall dispatch (keeps ps4-libs off ps4-memory; blanket impl in core over the trait core owns). SAFE FALLBACK: no source wired (headless/tests) → falls back to IdentityMem (safe: no wired VM = no untrusted guest, regs = real host test array). #3: VirtualMemoryManager::read_bytes_ranged default now returns Err('ranged read not implemented') instead of delegating to unbounded read_bytes (loud fail on missing override). Regression test out_of_bounds_regs_emits_nothing_and_never_over_reads (7-dword tail straddles mapped end → 0 PM4, no bind; in-bounds DOES bind). test-hooks feature on ps4-core for clear_bounded_read; TEST_LOCK serializes. Verify: workspace 181 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
