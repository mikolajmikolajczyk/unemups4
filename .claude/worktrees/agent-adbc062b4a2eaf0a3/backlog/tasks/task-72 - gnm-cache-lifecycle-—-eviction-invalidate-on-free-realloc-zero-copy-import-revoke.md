---
id: TASK-72
title: >-
  gnm/cache: lifecycle — eviction + invalidate-on-free/realloc + zero-copy
  import revoke
status: Done
assignee: []
created_date: '2026-07-12 06:01'
updated_date: '2026-07-12 22:18'
labels:
  - gpu
  - gnm
  - cpu
dependencies:
  - TASK-71
priority: medium
ordinal: 71000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review findings #3 + #5 (cache lifecycle; both need a guest free/unmap → cache signal that does not exist yet). #3: entries are only inserted, never removed (cache/mod.rs:262); keyed by (addr,size,layout). Guest frees + reallocs the same addr/size/layout (common PS4 sceKernelAllocateDirectMemory reuse) → a clean hit returns the OLD ResourceId → old/possibly-freed backend buffer → wrong data or use-after-free. Also unbounded growth + next_id (u32, cache/mod.rs:214) has no eviction so it can only grow. #5: a zero-copy imported entry (imported=true) suppresses ALL re-upload/dirty logic and there is NO unimport path (drain_dirty + invalidate_range both skip imported); guest releaseDirectMemory/munmap on an imported garlic range → the backend's external-memory vk buffer dangles into freed host pages → GPU reads freed memory. FIX: (a) add a cache eviction/invalidation entry point keyed by a freed guest range that drops the entry, tells the backend to destroy the resource (or defers destroy behind the frame fence), unwatches the DirtySource range, and for imported entries revokes the import; (b) WIRE it to the guest free/unmap path (sceKernelReleaseDirectMemory / munmap in kernel/libs) so the cache actually receives the signal. Decide eviction policy for plain growth (LRU/size cap) or document why unbounded-until-free is acceptable for the phase-4 corpus. Coordinate destroy-timing with the display thread (don't free a vk resource the GPU may still read this frame).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 freeing a guest range evicts overlapping cache entries: a subsequent get() for the same key mints a NEW id + re-creates (no stale-id clean hit) — unit test
- [x] #2 an imported entry whose range is freed is revoked (backend told to drop the import; no dangling external-memory buffer)
- [x] #3 the free→cache-invalidation signal is wired to the guest free/unmap path (not just an unused API)
- [x] #4 resource destroy is fence-safe (not freed while the GPU may still read it this frame) OR documented as deferred with the hook point
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add BackendCmd::FreeResource + MemoryFreeSink registered-seam in ps4-core; ResourceCache::free_range drops overlapping entries/unwatches/revokes imports emitting FreeResource cmds; impl MemoryFreeSink in gnm over driver()+PresentSink; wire process.munmap->notify_free; backend fence-safe FreeResource teardown; register sink at boot; document unbounded-until-free growth policy; independent unit tests.
<!-- SECTION:PLAN:END -->

Implementation Notes:
--------------------------------------------------
DONE 2026-07-13 (feat/task-72, uncommitted for review). Entry point: ResourceCache::free_range(addr,size,dirty,out) (cache/mod.rs) — drops every entry overlapping the freed range, appends BackendCmd::FreeResource{id} per evicted entry (both copy buffers and imports; the sole unimport path since drain_dirty/invalidate_range skip imported), and unwatches the range against the DirtySource. New Vulkan-free seam in ps4-core::gpu: BackendCmd::FreeResource{id} + MemoryFreeSink trait (notify_free) with register_memory_free_sink/memory_free_sink() registered-singleton mirroring register_present_sink. WIRING: kernel Process::munmap (backs both munmap + sceKernelReleaseDirectMemory) fires memory_free_sink().notify_free on a successful unmap after dropping the memory write lock; GnmMemoryFreeSink (crates/gnm/src/free_sink.rs) impls it — locks driver() (guest-thread caller, honors the display-thread-never-locks invariant), calls GnmDriver::free_resource_range→cache.free_range, then ships the FreeResource cmds over PresentSink::run_command_list. Registered at boot in app/unemups4/main.rs beside register_present_sink (app gained a ps4-gnm dep). AC#4 fence-safe destroy: AshBackend::free_resource (crates/gpu/src/backend.rs) removes the id from resources/imported and waits on the in-flight draw_fence before destroy_buffer/free_memory (copy) or destroy_imported_buffer (revoke); unknown id = no-op. Eviction policy: unbounded-until-free, NO LRU/size-cap — the guest owns each GPU buffer's lifetime and frees before reuse, so free-driven evict tracks the guest allocator exactly and bounds the cache to the guest's live GPU allocation (documented in cache/mod.rs module doc; a size-capped LRU layers on the same path later if a title streams unbounded distinct ranges). Independent tests (cache/tests.rs): free_evicts_copy_entry_realloc_mints_new_id (get→id1, free→one FreeResource(id1)+unwatch, get→id2≠id1 with fresh CreateBuffer), free_revokes_imported_entry (import→id1, free→FreeResource(id1), re-get→fresh id + re-ImportBuffer, proving the imported entry was dropped not stranded), free_evicts_only_overlapping_entries, free_unmatched_or_empty_range_is_noop — expected state reasoned independently (id2≠id1, command kind counts), not captured from the production path. GATE: build OK; ps4-gnm+ps4-core+ps4-gpu 165 pass; clippy -D warnings 0 lints (9 warnings are SDK-not-found build-script notices); fmt --check clean; run_examples check 6/6. Vulkan-free confirmed: cargo tree -p ps4-gnm has no ash/winit/gpu-allocator; no task-NN/P4-NN in source comments.
