---
id: TASK-156
title: >-
  gpu: Celeste splash background gradient too bright/saturated vs real dark-navy
  (color space now correct — likely guest content or upstream stage)
status: To Do
assignee: []
created_date: '2026-07-17 10:11'
labels:
  - gpu
  - gnm
  - celeste
  - retail
  - color
dependencies: []
priority: medium
ordinal: 162000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After the color-pipeline fix (merge a62a614: R↔B channel swap + proper sRGB linear-space compositing), Celeste's splash renders with CORRECT color space — the snow particles + 'Matt Makes Games Inc.' text now composite WHITE (were gradient-tinted), and the hue is blue/navy (was warm orange), PNG-oracle confirmed. But the BACKGROUND gradient is still notably brighter + more saturated (vivid blue->magenta) than the real Celeste splash, which is a DARK deep-navy/near-black with a subtle magenta glow. Since the white foreground proves the compositing color space is now correct (gamma decoded once on sample, encoded once at present via _SRGB swapchain), the remaining brightness is NOT a wrong-space blend. Prime suspects: (a) the guest's actual gradient content at this animation frame (the splash animates — the maintainer's dark reference may be a different moment / earlier-darker frame); (b) an UPSTREAM stage we don't model (guest tonemap/exposure, a fade/alpha animation, or the gradient drawn without an intended darkening multiply); (c) an additive layer over-accumulating. Method: RENDERDOC per-draw capture is the decisive tool here (inspect the gradient texture content, the per-layer blend accumulation, any tonemap draw) — the maintainer is setting up RenderDoc (XWayland: QT_QPA_PLATFORM=xcb qrenderdoc + WINIT_UNIX_BACKEND=x11 for the emu). Also compare more animation frames to the real splash. Do NOT add a present-shader brightness band-aid. Relates a62a614, task-154 (color), the sRGB color management.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Root-caused why the background is brighter than real (guest content vs upstream stage vs additive), ideally via RenderDoc per-draw
- [ ] #2 Fix (if ours): PNG oracle shows the dark-navy background matching real Celeste, WITHOUT a present-shader band-aid
<!-- AC:END -->
