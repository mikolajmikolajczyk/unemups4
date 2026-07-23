---
id: TASK-159
title: >-
  gpu/videoout: generalize scanout channel-order model + register-buffers error
  path (code-review)
status: To Do
assignee: []
created_date: '2026-07-17 11:32'
labels:
  - gpu
  - videoout
  - review
dependencies: []
priority: medium
ordinal: 165000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review findings on the videoout scanout path: (1) crates/gpu/src/backend.rs scanout_swap_rb hardcodes pixel_format == 0x80000000 as the ONLY swap case — other BGRA/sRGB SceVideoOutPixelFormat variants (e.g. 0x80002200 seen in libscevideoout's own encode test) are treated as no-swap → R<->B swapped for those titles; and present() falls back to swap=true (0x80000000) when current_target's buffer is unregistered → an RGBA title (A8B8G8R8) flipping an unregistered/mistimed index gets R<->B swapped. Replace the magic-constant equality with a general SceVideoOutPixelFormat channel-order + sRGB decode; pick a safe (no-swap) fallback for unknown/unregistered. (2) crates/kernel/src/bridge.rs video_out_register_buffers dropped the old EFAULT (Err(14)->-1) return: it now always returns Ok(0) even if every list entry faults and zero buffers register — a guest trusting the success code flips an unregistered index. Return an error (or at least a diagnostic) when no buffer registers. Dormant for Celeste (0x80000000, valid list) but latent for other titles.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 channel-order/swap derived from a general pixelFormat decode (not ==0x80000000), safe fallback; register_buffers signals failure when nothing registers
<!-- AC:END -->
