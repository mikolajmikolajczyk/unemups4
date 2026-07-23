---
id: TASK-2
title: 'SPIKE: guest_base identity-mapping support in x86jit'
status: Done
assignee: []
created_date: '2026-07-09 15:05'
updated_date: '2026-07-09 16:30'
labels:
  - migration
  - x86jit
  - spike
dependencies: []
priority: high
ordinal: 2000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Keystone spike — gates all downstream tasks. In the x86jit repo (~/src/x86jit): add guest_base: u64 (default 0) to HostRam/Reserved memory model; translation host = ptr + (g - guest_base) done as usize arithmetic (never materialize a null-adjacent pointer); reject map() below guest_base; bake numeric base into cranelift codegen. Differential tests with guest_base=0 against existing Reserved tests. mmap pattern: x86jit-linux/src/hostmem.rs::reserve but mmap(0x10000, span-0x10000, RW, PRIVATE|ANON|NORESERVE|MAP_FIXED_NOREPLACE). Enables unemups4 identity mapping (host addr == guest addr) so all raw guest-pointer derefs in syscall handlers and GPU keep working unchanged.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 x86jit test: Reserved VM with guest_base=0x10000, map at 0x400000, write mov eax,42; hlt, run -> Exit::Hlt with RAX==42
- [x] #2 embedder-side identity proven: unsafe { *(0x400000 as *const u8) } == 0xB8 in the same test
- [x] #3 full x86jit suite green under both interpreter and cranelift backends
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Spike executed in ~/src/x86jit on branch feat/guest-base-identity. Design: add guest_base:u64 (default 0) to HostRam -> Memory; backing indexing subtracts guest_base ((addr-guest_base) as usize) via a single host_off() helper; regions/map keep guest addresses and now reject addr<guest_base. JIT: MemCtx gains guest_base; checked_addr subtracts it before the bounds check (offset trick), emitting byte-identical code when guest_base==0. RawStrMem/RawFpMem gain guest_base. New reserve_at(guest_base,span) mmap helper in x86jit-linux (MAP_FIXED_NOREPLACE|NORESERVE). Tests: identity test (guest_base=0x10000, map 0x400000, mov eax,42;hlt -> RAX==42 + embedder-side *(0x400000)==0xB8) under both backends; existing guest_base=0 differential tests unchanged. Leaving unemups4 backlog edits uncommitted.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE in x86jit repo, branch feat/guest-base-identity, commit 9b490a1 (GPG-signed, NOT merged/pushed — coordinator merges). Design: HostRam.guest_base: u64 (default 0) -> Memory.guest_base; guest space [guest_base, span); all backing indexing via host_off(addr) = (addr - guest_base) as usize (integer arithmetic, never a null-adjacent pointer); map() rejects below-base (MapError::OutOfBounds). JIT: guest_base baked as a compile-time constant threaded like the mmio window (Backend::materialize/materialize_region + TierUpRequest); cranelift checked_addr emits the below-base reject + isub rebase ONLY when guest_base != 0, so guest_base==0 codegen is byte-identical (zero perf change). MemCtx grew guest_base at offset 72 (append-only ABI); JIT string/x87/fxstate helpers rebase via RawStrMem/RawFpMem.guest_base. NEW unemups4 entry point: x86jit_linux::hostmem::reserve_at(guest_base, span) — mmap(guest_base, span-guest_base, RW, PRIVATE|ANON|NORESERVE|MAP_FIXED_NOREPLACE), asserts the kernel honored the exact address, returns HostRam with guest_base set; pair with VmConfig::reserved(span) + Vm::with_backend_host_ram. AC1+AC2: x86jit-tests/tests/guest_base.rs — reserve_at(0x10000, 8MiB), map 0x400000 RX, write B8 2A 00 00 00 F4, run -> Exit::Hlt RAX==42 on BOTH interpreter and cranelift; unsafe { *(0x400000 as *const u8) } == 0xB8 asserted in the same test; below-base access traps UnmappedMemory{addr:0x8000} on both backends. AC3: full suite (cargo nextest run -E 'not binary(fuzz_robustness)') 305/305 green covering interp+jit variants incl. differential/smc/superblock/mt/guard_pages; clippy --all-targets -D warnings clean; fmt clean. Caveats for downstream tasks: (1) deep_copy/fork of a guest_base!=0 memory returns None (unemups4 never forks); (2) SMC CODE_WINDOW tracks guest addrs < 4 GiB — fine, PS4 image/code is low, 16 GiB heap holds no code; (3) reserve_guarded has no guest_base variant yet (guard pages deferred per plan task 9 note).
<!-- SECTION:NOTES:END -->
