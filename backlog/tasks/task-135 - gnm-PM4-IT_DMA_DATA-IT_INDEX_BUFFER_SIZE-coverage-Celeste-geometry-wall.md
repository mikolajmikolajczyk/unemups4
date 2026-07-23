---
id: TASK-135
title: 'gnm: PM4 IT_DMA_DATA + IT_INDEX_BUFFER_SIZE coverage (Celeste geometry wall)'
status: Done
assignee: []
created_date: '2026-07-16 11:03'
updated_date: '2026-07-16 11:47'
labels:
  - gnm
  - celeste
  - retail
  - gpu
dependencies: []
priority: high
ordinal: 141000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Celeste (CUSA11302) now boots Mono + submits GNM PM4 on main (post fcb7402), but the frame is uniform WHITE — no geometry — and the process SIGSEGVs shortly after submit. Live-run + PNG oracle (2026-07-16, main 65df256) show the executor logs 'unhandled PM4 opcode' for IT_DMA_DATA(0x50) + IT_INDEX_BUFFER_SIZE(0x13) (+ benign IT_NOP 0x10). These carry index/vertex-buffer setup, so the following IT_DRAW_INDEX_* has no geometry -> nothing renders. Dispatch site: crates/gnm/src/exec.rs:160-224 (Type3 match); unhandled fallback exec.rs:1403. Opcode consts in crates/gnm/src/pm4/opcodes.rs (IT_INDEX_BUFFER_SIZE=0x13, IT_DMA_DATA=0x50). NOTE: this file (exec.rs) also carries task-56 step5 readback work — sequence after that merges to avoid conflict.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 IT_INDEX_BUFFER_SIZE(0x13) decoded: sets index-buffer max-size/count state consumed by the following IT_DRAW_INDEX_* (matches GFX6 semantics)
- [x] #2 IT_DMA_DATA(0x50) decoded + executed: the guest DMA copy/fill lands in guest memory via the BOUNDED write seam (SMC-observed, never raw IdentityMem store); L2/GDS/register variants handled or cleanly deferred
- [x] #3 IT_NOP(0x10) silently skipped (not logged as unhandled)
- [x] #4 headless: a PM4 stream with IT_INDEX_BUFFER_SIZE + IT_DMA_DATA + IT_DRAW_INDEX_* decodes + drives the draw with correct index/vertex state (no 'unhandled' log for these three)
- [x] #5 live: Celeste re-run past submit — capture whether the frame gains geometry (PNG oracle) and root-cause / capture the post-submit SIGSEGV RIP (task-113.2 reporter or debug run)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 7d690da). 0x13/0x50/0x10 decoded in exec.rs; IT_DMA_DATA mem->mem via bounded_read+write_guest seam, register/GDS variants (all Celeste's, DAS=1, dst~0x3022c) deferred; IT_INDEX_BUFFER_SIZE->IndexState.max_size clamps offset draw; IT_NOP skipped. 197 gnm tests (3 new), clippy+fmt clean. LIVE: unhandled-PM4 wall GONE; frame still WHITE (PNG oracle) + post-submit SIGSEGV = SEPARATE downstream Graphics::GraphicsSystem::DrawPrimitives/Present call-time crash -> filed task-137. Lead: mem->register DMA may program draw registers (deferred today) -> investigate after crash clears.
<!-- SECTION:NOTES:END -->
