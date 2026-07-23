---
id: TASK-226
title: >-
  gpu: bind cache buffers at a byte OFFSET so a ring's sub-ranges reuse one
  buffer instead of minting a new one per batch
status: To Do
assignee: []
created_date: '2026-07-22 11:22'
labels:
  - gpu
  - perf
dependencies: []
ordinal: 231000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-223 established that Celeste's dynamic geometry comes out of a ~172 KiB vertex ring whose V# base is the write cursor and whose num_records spans the cursor to the end of the ring, so the cache key (addr, size, layout) changes on EVERY batch and cannot hit by construction. 100% of vertex misses are new_base, and 100% of those ranges lie wholly inside a live cache entry — the bytes are already on the GPU; only a byte offset is missing. On top of that, one draw resolves three vertex streams from the same interleaved data whose V# bases differ only by the attribute offset (+0/+12/+16, identical size), so ONE guest buffer becomes three entries, three CreateBuffers and three uploads of the same 43-48 KiB.

task-223 made the remaining creation nearly free (pooled suballocator, ~0.5-0.8 us each) and bounded the population, so this is no longer a latency emergency. What it would still buy: roughly 20 creates/flip gone entirely, about two thirds of the vertex UPLOAD volume gone (3.58 MiB/flip measured in the heavy scene), and a much smaller live entry set.

The work is a change to the descriptor ABI, which is why task-223 did not do it:
- BindVertexBuffer / BindStorageBuffer / BindConstBuffer / DrawIndexed carry no offset today.
- Every descriptor is written with VK_WHOLE_SIZE; an SSBO/UBO offset must respect minStorageBufferOffsetAlignment, which is a MoltenVK portability question (Bloodborne north star) and needs a boot-resolved guest-side mirror like the existing ImportProbe, plus a create-a-fresh-buffer fallback when the offset does not align.
- The cache needs a containment lookup (find a live entry of the same kind whose range covers the request) alongside the exact-key lookup, and must decide what a dirty hit re-uploads — the whole containing range, or just the window.

Correctness bar: a containing entry's dirty tracking already watches the whole range, so a write anywhere in it re-uploads it; that is correct but changes upload SIZE distribution, which must be measured, not assumed. Needs the maintainer's eyes on gameplay before it is called done.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the cache finds a live entry containing a requested sub-range and binds it at an offset instead of creating a new buffer
- [ ] #2 descriptor offset alignment is resolved from device caps, with a create-a-fresh-buffer fallback when a range cannot be bound at its offset
- [ ] #3 measured in gameplay: CreateBuffer count, upload MiB/flip and frames-per-window, before and after
- [ ] #4 maintainer confirms the scene still renders correctly
<!-- AC:END -->
