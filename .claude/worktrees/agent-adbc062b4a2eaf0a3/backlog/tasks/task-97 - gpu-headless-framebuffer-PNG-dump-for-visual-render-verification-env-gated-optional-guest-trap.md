---
id: TASK-97
title: >-
  gpu: headless framebuffer->PNG dump for visual render verification (env-gated
  + optional guest trap)
status: Done
assignee: []
created_date: '2026-07-12 21:04'
updated_date: '2026-07-12 21:53'
labels:
  - gpu
  - test
dependencies: []
priority: high
ordinal: 96000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Process gap that cost real time this session: render correctness was 'verified' via log/heuristic proxies (e.g. counting 'imported guest framebuffer' lines) which LIE — a proxy showed success while the window was black AND an embedded-path regression went unnoticed. FIX: an env-gated headless framebuffer->PNG dump so the orchestrator/agent can Read the PNG and visually judge the rendered output itself (the Read tool renders images), with NO display. Design: UNEMUPS4_DUMP_PNG=<path> (or a dir + frame counter) -> after present/flip, read back the presented image (or the drawn texture_image) to an RGBA PNG. Optional refinement: a guest 'capture trap' marker in the test ELFs (a distinguished sceKernelDebugOutText string or a debug syscall) that signals WHICH frame to dump (the draw+flip frame, not a random one), so single-submit homebrew reliably captures the drawn frame before exit. Reuse the diff_harness readback path (it already reads pixels back). Enables self-verification of task-96 triangle + any future GPU draw without bugging the maintainer's eyes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 UNEMUPS4_DUMP_PNG (env-gated) writes the presented/drawn frame to a readable RGBA PNG in a headless run; off by default (oracle baselines unchanged)
- [x] #2 the PNG of ps4-pm4-test embedded R/G is a visible gradient (not black); ps4-gcn-triangle's PNG shows the triangle when it renders
- [ ] #3 optional: a guest capture-marker selects the draw frame for single-submit homebrew
- [ ] #4 1
- [ ] #5 2
<!-- AC:END -->

## Notes

Implemented as an env-gated swapchain-image readback in `crates/gpu/src/backend.rs` `dump_present_png` (+ `write_rgba_png`, a self-contained uncompressed-zlib PNG encoder — no `png` crate added). `UNEMUPS4_DUMP_PNG` accepts a file (overwritten each flip) or a directory (`frame_NNNN.png` per flip). Swapchain images gain `TRANSFER_SRC` usage (`crates/gpu/src/vulkan.rs`); readback runs after the present submit but BEFORE `queue_present`, while the image is still owned by us in `PRESENT_SRC`, so it captures the true scanout (present-clear + composited quad) for both the embedded-draw and pure-softgpu paths. Off by default; oracle baselines unchanged; run_examples 6/6 green. Verified: triangle PNG shows the pink triangle, pm4-test PNG shows the R/G gradient. AC#3 (guest capture-marker) left unimplemented — unnecessary: the small single-submit frame set already contains the drawn frame and the readback captures it reliably.
