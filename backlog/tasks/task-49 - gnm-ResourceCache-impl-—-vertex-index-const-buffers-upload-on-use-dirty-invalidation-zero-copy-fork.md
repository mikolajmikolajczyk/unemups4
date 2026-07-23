---
id: TASK-49
title: >-
  gnm: ResourceCache impl — vertex/index/const buffers, upload-on-use, dirty
  invalidation, zero-copy fork
status: Done
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-11 18:14'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-48
priority: medium
ordinal: 48000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Implement §8.1/§8.2 cache: get(key,mem,backend,dirty)->ResourceId with first-use policy (try try_import_host_range, else create_resource+upload+watch), clean-hit no-op, dirty-hit re-upload, keyed (addr,size,layout). Drain DirtySource once per submit, mark overlapping entries dirty. Turn ResourceCache trait stub into concrete struct (adjust stub sig to §8.1 get(key,mem,backend,dirty)). Requires GpuBackend::{create_resource,upload} real in AshBackend for plain buffers (small backend add) — or, per channel model, cache bookkeeping on guest thread emitting BackendCmd::{CreateBuffer,UploadBuffer} with display thread owning allocation; task decides per doc-2 §3 data-list model. Does NOT handle textures/tiling (layout enum grows, only linear buffer kinds impl); does NOT readback.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: MockBackend — first use = 1 create+upload; clean reuse = 0 uploads; dirty range → exactly overlapped entries re-upload (§6 linchpin)
- [ ] #2 headless: same bytes as two layouts = two entries
- [ ] #3 headless: zero-copy fork — mock returning Some(import) skips upload; None falls back
- [ ] #4 onion/garlic hook point exists as optional policy input (§C5 seam), defaults copy-side
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fable phase-4 review finding #3 [decide at KICKOFF, before task-51/52 encode BackendCmd variants]: the doc-2 §8.1 signature get(key, mem, &mut dyn GpuBackend, dirty)->ResourceId is SYNCHRONOUS, but at runtime the cache runs on the GUEST thread (only &dyn PresentSink there) while the sole GpuBackend + id minting (backend.rs next_id) live on the DISPLAY thread across a one-way channel. Two seams collide: fire-and-forget BackendCmds can't return backend-minted ids, and try_import_host_range (AC #3 zero-copy fork) needs a synchronous Option answer that depends on device caps only the display thread knows (ctx.ext_mem_host, alignment, backend.rs:202-231). DECIDE the id-ownership model ONCE at kickoff: recommended = guest-side ResourceId allocator + display-side id→vk map (so ids mint guest-side, no round-trip), and mirror import-capability guest-side (or answer via the existing blocking handshake). AshBackend::create_resource/create_target as currently shaped (backend-minted ids) is the WRONG shape for the channel model — reshape it. task-51/52's BackendCmd variants MUST agree with whatever this task decides.

---

DONE 2026-07-11 (feat/task-49 @<prior-history>, merged <prior-history>). ID-OWNERSHIP DECISION (Fable #3): adopted the recommendation — ResourceIds mint GUEST-SIDE (ResourceCache owns a monotonic counter), handed INTO the backend which keeps a display-side id→vk map. No round-trip on the one-way channel. Import capability mirrored guest-side via an ImportProbe trait so get() stays synchronous. Documented in TWO places constraining task-51/52: ps4-gnm::cache module doc ("Id ownership across the guest/display thread boundary") + GpuBackend::create_resource doc in core/gpu.rs. Reshaped GpuBackend: create_resource(id, desc)->() (was minting), try_import_host_range(id, ptr, size)->bool (was ->Option<ResourceId>). BackendCmd UNCHANGED (kept Copy) — no buffer op lands in phase 3.5; task-51/52 add CreateBuffer/UploadBuffer carrying the guest-minted id (flagged in module doc). Files: core/gpu.rs, gnm/cache/mod.rs (trait stub->concrete struct + ResourceKey/ResLayout/Coherence/ImportProbe/CoherenceSource/CachePolicy), gnm/cache/tests.rs, gpu/backend.rs (real create/upload for linear bufs + id->CacheBuffer map), gpu/vulkan.rs (create_cache_buffer/upload_cache_buffer, all vk in leaf). Scope: linear vertex/index/const buffers only, no textures/tiling/readback. All 4 ACs green (first-use=1c+1u / clean=0 / dirty=exactly-overlapped re-upload; two-layouts=two entries; import skips upload else falls back; policy defaults copy-side). Verify: gnm+core+gpu 83 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined main gate: 28 suites ok, oracle 6/6.
<!-- SECTION:NOTES:END -->
