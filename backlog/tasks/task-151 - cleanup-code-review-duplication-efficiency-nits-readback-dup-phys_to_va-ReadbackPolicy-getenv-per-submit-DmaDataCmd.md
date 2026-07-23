---
id: TASK-151
title: >-
  cleanup: code-review duplication/efficiency nits (readback dup, phys_to_va,
  ReadbackPolicy getenv-per-submit, DmaDataCmd)
status: To Do
assignee: []
created_date: '2026-07-16 16:40'
labels:
  - cleanup
  - tech-debt
  - gpu
  - gnm
dependencies: []
priority: low
ordinal: 157000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Low-priority code-review (2026-07-16) cleanup, none are bugs: (1) copy_rt_to_host (gpu/backend.rs) duplicates dump_present_png's image->staging-buffer one-shot copy (~80 lines each, already diverged on fence timeout) -> extract VulkanContext::copy_image_to_host. (2) DirectMemory::phys_to_va (kernel/process.rs) is private while vm_backend.rs inlines POOL_BASE+off at 4+ sites -> make phys_to_va pub, call it from both. (3) ReadbackPolicy::from_env() (gnm/exec.rs Executor::new) does a getenv+String parse per sceGnmSubmit (~60/s) for a value that never changes -> cache in OnceLock like safe_gain()/TRACE. (4) IT_DMA_DATA command-word bit masks (exec.rs dispatch_dma_data) are inlined + re-encoded independently in the test helper -> extract a DmaDataCmd{byte_count,src_is_reg,dst_is_reg} parse/encode struct. (5) fs.rs local read_cstr dup (being fixed under the code-review batch). (6) recompile.rs fetch recomputes index*stride/UDiv per component in the MUBUF loop -> hoist dword_base out of the loop (SPIR-V size only; GPU CSEs it). (7) copy_rt_to_host .to_vec()+tile() = 3x linear_size transient for a 1080p RT -> re-tile from the mapped ptr. (8) readback loop does one submit+fence-wait per RT -> batch into one cmdbuf. Pick off opportunistically; no correctness impact.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The listed duplications are consolidated to a single source (readback copy, phys_to_va, DmaDataCmd) OR consciously left with a note
- [ ] #2 ReadbackPolicy::from_env cached (OnceLock); no per-submit getenv
<!-- AC:END -->
