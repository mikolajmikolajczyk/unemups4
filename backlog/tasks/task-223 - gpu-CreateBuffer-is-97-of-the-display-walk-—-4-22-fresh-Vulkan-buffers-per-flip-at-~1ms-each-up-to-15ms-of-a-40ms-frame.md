---
id: TASK-223
title: >-
  gpu: CreateBuffer is 97% of the display walk — 4-22 fresh Vulkan buffers per
  flip at ~1ms each, up to 15ms of a 40ms frame
status: Done
assignee: []
created_date: '2026-07-22 10:36'
updated_date: '2026-07-22 11:28'
labels:
  - gpu
  - perf
dependencies: []
priority: high
ordinal: 228000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-222 fixed the per-upload map/unmap and cut UploadBuffer from 11140 ns to 1822 ns each. That exposed what was underneath, and it is far larger. Per-variant breakdown of the display-thread command walk, Celeste GAMEPLAY (maintainer run, per-window rows):

    cmd walk [window]: 369 cmds/flip, 7.845 ms/flip attributed, 1.65 MiB/flip uploaded
      CreateBuffer     7.9/flip   7.625 ms/flip (97.2%)  avg 960577 ns each
      UploadBuffer    96.6/flip   0.176 ms/flip ( 2.2%)  avg   1822 ns each
      BindStorageBuffer 54.9/flip 0.014 ms/flip ( 0.2%)
      ...everything else under 0.2% combined

Consistent across every window of the run: CreateBuffer is 96-98% of the walk, at 4 to 22 creations per flip, 0.70 to 1.62 ms EACH, totalling 6.4 to 15.5 ms per flip in a frame of 33-47 ms.

Two separate problems, and both matter:

1. WE CREATE THEM AT ALL. Something like 8-22 fresh buffers every frame means the resource cache is missing on geometry the guest re-submits each frame. Find out what the cache keys on and why a Celeste frame's vertex/index/uniform buffers do not hit. If the key includes a guest address that rotates per frame, the cache can never hit by construction.

2. EACH ONE COSTS A MILLISECOND. That is pathological on its own — a vkCreateBuffer plus vkAllocateMemory is normally tens of microseconds. One millisecond suggests a dedicated VkDeviceMemory allocation per buffer, and drivers both cap the number of live allocations and degrade as it grows. Even with the cache fixed, whatever creation remains should come from a pooled suballocator rather than its own device allocation.

Fix the cache first if it is fixable — a buffer never created costs nothing — then pool whatever creation genuinely remains.

Scale for comparison: this single item is larger than task-217 (PM4 walk, 2.29 ms), task-205 (draw fence, 1.42 ms) and task-206 (transient objects, 0.69 ms) combined, and it was invisible until task-222 added per-variant attribution.

Measure cmd_walk, the CreateBuffer count and per-call cost, and frames-per-window, before and after. Note the pattern this investigation keeps hitting: cost removed from one phase has repeatedly MOVED rather than shortened the frame — report the frame rate too, and say so plainly if the row shrinks and the frame does not. Attract will not show this (it creates 1.5 buffers/flip at 32 us); only gameplay does, which needs the maintainer.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 why the resource cache misses on per-frame guest buffers is established from the code and the data, not assumed
- [x] #2 buffers that the guest re-submits each frame are reused rather than recreated, or the notes explain precisely why they cannot be
- [x] #3 whatever creation remains does not take a dedicated device allocation per buffer
- [x] #4 measured before/after in GAMEPLAY: cmd_walk, CreateBuffer count and per-call cost, and frames-per-window — all reported even if the frame rate does not move
- [x] #5 build + clippy clean, cargo test --workspace green; maintainer confirms the scene still renders correctly
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. MEASURE FIRST (AC #1). Add env-gated (UNEMUPS4_PROFILE) relaxed-atomic counters:
   - guest side (ps4-gnm cache): per-layout get/clean-hit/dirty-hit/first-use-create, plus miss classification — addr never seen before vs addr seen with a DIFFERENT size vs exact key seen and evicted. This says whether the key rotates by address (ring buffer) or by size (varying num_records/index_count).
   - display side (ps4-gpu present_profile, extending task-222's CmdStats): live cache-buffer count + live device-allocation count, so per-CreateBuffer cost can be plotted against how many allocations are live.
   Report from profiler_dump.rs as per-window rows next to 'cmd walk [window]'.
2. Read the numbers from a run I can reach (attract/title) and hand the maintainer the same command for GAMEPLAY. Establish, not assume, why the key misses.
3. FIX (1) — stop creating: whatever the classification says. If it is size rotation at a stable address, bucket the size in the key + allocate the bucket. If it is address rotation (a MonoGame dynamic ring), the key cannot hit by construction and the notes will say so — then the count is bounded by an eviction/recycle path instead.
4. FIX (2) — make the remaining creation cheap (AC #3): today create_resource does one vkCreateBuffer + one dedicated vkAllocateMemory + one vkMapMemory per buffer, and nothing is ever freed, so live allocations grow without bound and each new one costs ~1 ms. Replace with a pooled suballocator: large HOST_VISIBLE|HOST_COHERENT blocks mapped once, per-resource vkCreateBuffer bound at a suballocated offset, plus a size-bucketed free list so a recycled buffer costs zero Vulkan calls. Frees go through a deferred queue released only after the draw fence the free_resource path already waits on, so no buffer is recycled while the GPU may still read it.
5. VERIFY: build, clippy, cargo test --workspace, profiler-OFF boot, and before/after rows verbatim — cmd_walk, CreateBuffer count + per-call cost, AND frames-per-window. If the row shrinks and the frame rate does not, say so prominently.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed 2026-07-22, confirmed by the maintainer's eyes.

VISUAL REGRESSION EN ROUTE — worth recording, because it is the exact failure the task warned about and a green test suite did not catch it. An intermediate revision evicted by a 4-flip age TTL and called dirty.unwatch() on every evicted range. Dirty tracking is PAGE-granular, and a ring's windows share pages, as do the small per-frame constant buffers — so unwatching an evicted entry dropped write protection for entries still live, and stale bytes were served as current. The maintainer saw it as wrong textures and a wrong 3D mountain. The profiler caught it in the same run: constant buffers went from 4.1 dirty hits/flip to 3.4 CLEAN hits/flip.

Fix: only free_range unwatches (there the guest genuinely gave the memory back); trim never does. Pinned by a unit test, over_budget_evict_frees_the_entry_but_keeps_the_range_watched.

AC #1 — WHY THE CACHE CANNOT HIT, measured rather than assumed. Key is (addr, size, layout). Celeste's dynamic geometry comes from a ~172 KiB ring where the V# base is the write cursor and num_records spans cursor to end-of-ring, so size shrinks by exactly as much as addr advances (0x9afd52800/48000, 0x9afd53ac0/43200, ... minus 4992 B per batch). Both halves of the key move together — it is unique BY CONSTRUCTION and no keying change can fix it. 100% of vertex misses are new_base, 0% new_size, 0% recreate, and 100% lie wholly inside a live entry.

AC #2 — real reuse needs offset-carrying binds (no bind command has an offset; descriptors use VK_WHOLE_SIZE; SSBO offsets must respect minStorageBufferOffsetAlignment). Filed as task-226. Instead the population is bounded by a 64 MiB LRU byte budget in ResourceCache::trim. An age bound was tried FIRST and was worse — rings wrap on their own period, so a TTL evicted entries about to be hit and creates went 1.0 to 12.3/flip.

AC #3 — crates/gpu/src/buffer_pool.rs: 16 MiB HOST_VISIBLE|HOST_COHERENT blocks mapped once, per-resource vkCreateBuffer bound at a suballocated offset, freed regions recycled by power-of-two size class. Buffers are created at EXACTLY the requested size, never rounded to the class, because VK_WHOLE_SIZE descriptors would otherwise widen past the uploaded bytes and turn an out-of-bounds read from a robustness zero into the previous tenant's data.

AC #4 — measured, and independently reproduced on a second machine-run at matched upload volume (3.50 MiB/flip gameplay):

    cmd walk            12.113  ->  0.298 ms/flip
    CreateBuffer each  960577   ->    948 ns
    flip                 ~12.0  ->  5.510 ms
    fps                  29-30  ->  37.03

Creates per flip actually ROSE (7.9 -> 34.2); each is now a microsecond instead of a millisecond. Baseline per-call cost also climbed within a single run (59k -> 82k -> 173k -> 342k ns) as live device allocations grew past 1381 — that growth term is gone by construction, 14 pool blocks flat.

SAFETY AGAINST IN-FLIGHT GPU WORK: FreeResource for a cache buffer destroys nothing, it queues onto pending_recycle, drained at the START of the next command list. replay_command_list submits and waits on its fence inside the same call, so by the next list every draw that could reference it has completed. The deferral is REQUIRED, not merely conservative: an earlier draw in the list being walked may bind a buffer freed later in that same list. Images/RTs/imports keep the original immediate wait-then-destroy. Eviction cannot return stale ids — the key is removed, so a later get mints a fresh id and re-uploads current bytes.

AC #5 — maintainer played the title and confirms textures and the 3D mountain render correctly. Zero BUFCACHE STALE-HIT across a full independent run including gameplay.

STRUCTURAL CONSEQUENCE: the frame is now 27.0 ms = guest_exec 20.8 + flip 5.5 + 0.7. Guest CPU is 77% of it and the GPU submit path has stopped being the bottleneck.

Build clean, clippy clean for the crates touched, cargo test --workspace 576 green (575 + the new regression test), profiler-OFF boot verified.
<!-- SECTION:NOTES:END -->
