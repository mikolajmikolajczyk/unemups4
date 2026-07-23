---
id: TASK-148
title: >-
  Celeste: Mono aborts in direct-memory release bookkeeping
  (mono-mmap-orbis.c:219), NOT our munmap return
status: Done
assignee: []
created_date: '2026-07-16 14:53'
updated_date: '2026-07-16 16:19'
labels:
  - celeste
  - retail
  - mem
  - kernel
  - bug
dependencies: []
priority: high
ordinal: 154000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-146 investigation DISPROVED the premise: our munmap/sceKernelReleaseDirectMemory ALREADY returns 0 (success) on every call (verified live via per-call DBG: all '-> 0'). The Mono abort at mono-mmap-orbis.c:219 g_assert(res==0) is NOT triggered by our return value. Static disasm of the reconstructed eboot (fake-SELF; inner ELF PT_LOAD p_offset=0x4000 p_vaddr=0) shows the free idiom is 'call <import>; test eax,eax; js <cleanup>' — a NEGATIVE EAX is the error/short-circuit; EAX==0 takes the SUCCESS path into an indirect bookkeeping callback [0x2c3aa90] (mono_vfree tracking). The abort's rbp backtrace (frame#1 ret 0x16dd33) lands right AFTER that indirect call, i.e. the g_assert fires INSIDE the mono callback on the success path, downstream of a direct-memory release. Root cause is our direct-memory MODEL: sceKernelReleaseDirectMemory(start,len) treats the physical-offset 'start' as a guest VA and munmaps it (madvise DONTNEED fails ENOMEM for start=0x0 since 0x0 isn't in the arena; benign warn), so Mono's physical-offset<->virtual-mapping accounting is inconsistent and its internal invariant check aborts on the asset-streaming thread (Thread 5) ~mid-gameplay-load. The main render thread survives and keeps issuing GNM draws+flips; PNG still black (separate GPU wall). Fix likely needs a faithful direct-memory model: track AllocateDirectMemory physical offsets, map MapDirectMemory VA<->offset, and Release by offset (not blind munmap of 'start' as VA). Nondeterministic (1 of ~209 start=0x0 releases aborts) => timing/state dependent, consistent with an accounting-invariant race not a fixed wrong-return. Note: task-146 shipped a real improvement in crates/memory (unmap now removes overlapping/interior VMAs, not just exact-key; 'not tracked in VMA' flood demoted warn->debug; AC#1 untracked-unmap-returns-Ok test added) — keep it, but it does NOT clear this abort.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-16. FIXED. Root cause chain: (1) release munmapped physOffset as a VA -> desync; (2) real bug = Mono mono_vfree munmaps a sub-range INSIDE a still-live direct-memory region, and our VmBackend::unmap ran madvise(DONTNEED)+dropped the VMA, zeroing/freeing pages Mono still referenced -> nondeterministic heap corruption -> mono-mmap-orbis.c:219 g_assert(res==0) (res always 0 from us; garbage read from corrupted heap). Fix: faithful physical-offset direct-memory pool (va=POOL_BASE(0x9_0000_0000)+off, 5GiB, consts in core/kernel.rs; DirectMemory bump allocator in process.rs, offsets never reused; allocate=reserve-only, map=FIXED map+zero-fill fresh, release=bookkeeping-only no-op). CRUX: VmBackend::unmap now treats any unmap INSIDE the pool window as a total no-op (no madvise, no VMA drop) so Mono's interior sub-range frees never corrupt the region. Files: core/kernel.rs, kernel/process.rs (+bridge.rs), libs/mman.rs, memory/vm_backend.rs. Verified: 3 clean 50-65s Celeste runs, 0 mono aborts / 0 faults (was ~1/run), asset thread (tid5/ThreadId13) survives, ~800-1366 GNM draws + ~157-254 flips + 986 shader/tex binds per run. 436 workspace tests pass, clippy -D warnings clean, fmt clean. PNG STILL BLACK = separate GPU render wall (out of scope, mem/kernel lane). NOT committed.
<!-- SECTION:NOTES:END -->
