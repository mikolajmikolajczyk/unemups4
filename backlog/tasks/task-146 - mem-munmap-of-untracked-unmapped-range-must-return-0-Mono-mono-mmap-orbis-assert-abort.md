---
id: TASK-146
title: >-
  mem: munmap of untracked/unmapped range must return 0 (Mono mono-mmap-orbis
  assert -> abort)
status: Done
assignee: []
created_date: '2026-07-16 14:09'
updated_date: '2026-07-16 14:57'
labels:
  - mem
  - kernel
  - celeste
  - retail
  - bug
dependencies: []
priority: high
ordinal: 152000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Celeste blocker found after the x86jit VMASKMOVPS lift (pin 942d253). Mono's mono-mmap-orbis.c:219 asserts 'res == 0' after a munmap; our vm_backend returns a NON-ZERO error for a range not in the tracked VMA set (log floods 'Unmapping memory not tracked in VMA: 0x...' right before the assert), so Mono aborts -> int-0x44 stub at 0x982e26 -> Exception vector 68 -> Thread 5 fatal, ~50s in, before geometry. POSIX semantics: munmap of an unmapped/untracked (or partially-mapped) range SUCCEEDS (returns 0) — unmapping nothing is not an error; only bad alignment/params (EINVAL) fail. Fix: crates/memory vm_backend unmap should return Ok(0)/success for an untracked or partial range (remove whatever VMA overlap exists, madvise DONTNEED, and return 0) instead of erroring. Keep the 'not tracked' as a debug log at most (the flood suggests Mono frees sub-ranges / guard pages we never tracked as distinct VMAs). Verify: Celeste re-run past 0x982e26 (RUST_LOG=warn,ps4_kernel=info,ps4_libs=info); PNG oracle for geometry. Assets gitignored, never commit. Related: task-145 (previous Mono abort), doc-5 Case 18 (managed abort from our own stub on a spurious errno/assert).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 vm_backend unmap returns success (0) for an untracked / partially-tracked / already-unmapped range (POSIX munmap semantics); only genuinely invalid params fail
- [ ] #2 Mono's mono-mmap-orbis.c:219 'res == 0' assert no longer fires; the 0x982e26 abort on the Mono thread is gone
- [ ] #3 Live: Celeste re-run past this abort; report next wall + PNG (does geometry appear now?)
<!-- AC:END -->

## Notes
<!-- SECTION:NOTES:BEGIN -->
Investigation 2026-07-16: the task PREMISE ("our vm_backend returns a NON-ZERO error for an untracked range") is DISPROVEN. `vm_backend::unmap` already returned Ok(()) unconditionally (only warned); `process.munmap` maps Ok->Ok(0); all three free syscalls (sys_munmap / sce_kernel_munmap / sce_kernel_release_direct_memory) verified live to return 0 on EVERY call (per-call DBG, all '-> 0'). So AC#2 is NOT caused by our return value.

Root cause (see TASK-148): static disasm of the reconstructed eboot shows the guest free idiom `call <import>; test eax,eax; js <cleanup>` — NEGATIVE EAX is the error/short-circuit; EAX==0 takes the SUCCESS path into an indirect mono bookkeeping callback [0x2c3aa90]. The abort's rbp backtrace lands right AFTER that indirect call: the g_assert(res==0) fires INSIDE mono's tracking callback on the success path, downstream of a direct-memory release — a mono-internal invariant, not our syscall return. Our `sceKernelReleaseDirectMemory` blindly munmaps `start` as a VA (madvise ENOMEM for start=0x0), leaving mono's phys-offset<->VA accounting inconsistent -> nondeterministic abort (1 of ~209 start=0x0 releases) on the asset-streaming thread (Thread 5). Main render thread survives + keeps issuing GNM draws/flips; PNG still fully black (separate GPU wall).

AC#1 DELIVERED anyway (genuine improvement, tests added): `vm_backend::unmap` now removes ALL overlapping/interior VMAs (not just the exact-start key) so Mono's sub-range/guard-page frees clean up tracking; the "not tracked in VMA" warn flood is demoted to debug!; the permanent HLT gadget VMA is never evicted. New tests in crates/memory/tests/vm_backend.rs: unmap_of_untracked_range_returns_ok, unmap_of_interior_subrange_clears_overlapping_vma, unmap_never_evicts_the_gadget_page. cargo test -p ps4-memory -p ps4-cpu green (30), clippy/fmt clean. Real fix continues in TASK-148 (faithful direct-memory model).
<!-- SECTION:NOTES:END -->
