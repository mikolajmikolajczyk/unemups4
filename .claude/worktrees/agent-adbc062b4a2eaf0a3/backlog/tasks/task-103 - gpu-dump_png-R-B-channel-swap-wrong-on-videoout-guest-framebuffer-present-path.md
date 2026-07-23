---
id: TASK-103
title: >-
  gpu/dump_png: R<->B channel swap wrong on videoout/guest-framebuffer present
  path
status: Done
assignee: []
created_date: '2026-07-13 10:17'
updated_date: '2026-07-13 19:31'
labels:
  - gpu
  - tooling
  - png-oracle
  - bug
dependencies: []
ordinal: 102000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The UNEMUPS4_DUMP_PNG visual oracle (task-97) mis-orders color channels when dumping the guest-framebuffer/videoout present path. EVIDENCE: running the ps4doom (Doom) homebrew, the LIVE display is correct (red Freedoom title, cross-validated against shadPS4 AND the real Freedoom Phase 1 title screen), but the dumped PNG shows R<->B swapped (red->magenta, brown->teal, psychedelic). The GCN/triangle/texture dumps (AshBackend render path) come out CORRECT, so the bug is specific to the videoout present path's format at dump time. The readback's BGRA->RGBA swap heuristic (backend.rs:1965-1974) keys off ctx.swapchain_format via matches!(format, B8G8R8A8_UNORM|B8G8R8A8_SRGB); on the guest-framebuffer present it either sees a format that doesn't match the actual bytes, or the guest present writes a different channel order than the triangle path. IMPORTANT: this is a DEBUG-TOOL bug — the live render is correct — but it makes the orchestrator's self-verification oracle LIE about colors (already caused one false 'colors broken' alarm). Fix so the PNG matches the live display for the videoout present.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 dump_png of the ps4doom present matches the live display (red title, not magenta) — verified by eye against shadPS4/real Freedoom
- [x] #2 GCN/triangle/texture dumps remain correct (no regression on the AshBackend path)
- [x] #3 Root cause identified: whether ctx.swapchain_format is wrong for the videoout path or the guest present writes a different channel order; fix at the correct spot
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Root cause: swapchain negotiated A2B10G10R10_UNORM_PACK32 (packed 10-bit, HDR display); dump assumed 4x8-bit BGRA/RGBA. Replace the R<->B-swap heuristic with a per-format converter swapchain_to_rgba8 that unpacks 2-10-10-10. Verify doom PNG shows red (by eye), 8-bit paths byte-identical (no AshBackend regression), add unpack unit test.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-13. Root cause: swapchain = A2B10G10R10_UNORM_PACK32 (packed 10-bit, HDR display); dump assumed 4x8-bit. Added swapchain_to_rgba8() with 2-10-10-10 unpack; 8-bit paths byte-identical (no AshBackend regression). ps4doom dump verified red-not-magenta by eye. Unit test unpack_a2b10g10r10_orders_channels_rgba passes.
<!-- SECTION:NOTES:END -->
