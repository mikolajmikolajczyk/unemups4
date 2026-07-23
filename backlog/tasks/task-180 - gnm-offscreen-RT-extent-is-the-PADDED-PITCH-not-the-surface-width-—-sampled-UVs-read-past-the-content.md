---
id: TASK-180
title: >-
  gnm: offscreen RT extent is the PADDED PITCH, not the surface width — sampled
  UVs read past the content
status: Done
assignee: []
created_date: '2026-07-20 12:30'
updated_date: '2026-07-20 13:28'
labels:
  - gpu
  - gnm
  - rt
  - correctness
dependencies: []
priority: medium
ordinal: 184000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
derive_target sets width = pitch for TargetKind::Offscreen, but CB_COLOR0_PITCH is alignment-padded. Measured on Celeste's menu: the bloom targets are drawn with viewport (0,540,960,-540) while we create the RT image as 1024x576, and the scene target has viewport 1920x1080 against a 1920x1088 image. The guest's UVs span the CONTENT (960/1080), so a consumer sampling [0,1] reads ~6% past it into never-written padding — the composited image is slightly scaled and carries a border. CONFIRMED VISUALLY during task-179: with UNEMUPS4_X_ADDITIVE=1 the affected region ends at exactly 93.75% of the screen width, which is 960/1024. The RT's sampled extent should come from the surface/viewport extent, with pitch used only as the row stride.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Offscreen RT extent derives from the surface extent, with the padded pitch used only as row stride
- [x] #2 A consumer sampling the RT at UV 1.0 reads the last CONTENT texel, not padding
- [x] #3 Regression test covers a padded surface (e.g. 960 content / 1024 pitch)
<!-- AC:END -->

## Notes

**Landed and confirmed (2026-07-20).** The menu scene now covers the full screen — maintainer's
live oracle.

Two geometries were conflated in `derive_target`'s offscreen branch and are now separate:

- **allocation** = `pitch × rows`, both alignment-padded tile-max encodings. Still what `size`
  is computed from, so `TargetKind::Offscreen`'s aliasing range and the resource-cache key are
  byte-identical to before. The host image extent was never load-bearing for aliasing — the
  worry that shrinking the image would break it did not survive contact with the code.
- **content extent** = the viewport rect, clamped per-axis to the allocation. This is now
  `TargetDesc::width`/`height` and sizes the host RT image a consumer samples at UV [0,1].

**The decisive evidence was not a capture.** Celeste's blur constant buffers are `1/960` on
the horizontal pass and `1/540` on the vertical one: the guest normalises its UVs over the
CONTENT extent, while we were sizing the image to the padded 1024×576. UV 1.0 therefore read
into never-written padding, ~6.25% per axis per hop.

### The task's own numbers were partly wrong — worth reading before trusting this file

The 93.75% recorded in the description is the ONE-HOP figure. Celeste's bloom is a two-hop
chain, so the shrink compounds: 0.9375² = 0.8789, which is the ~87.8% width the maintainer
measured on the composited frame. The scene composite was short by 1080/1088 = 0.74%
vertically and 0% horizontally, which is why a separate observation of the same build showed
a texture rendering FULL SCREEN. Those two observations were never in conflict — they were
different draws in the same frame, and reconciling them is what confirmed the mechanism.

The measured 89.4% height still does not fully reconcile (predicted ~87.2%). The unverified
guess is that the last pass being the vertical blur smears the padding boundary into a soft
edge that measures wider than the sharp horizontal one. Recorded as an open loose end, NOT as
a finding.

### Known limitations, deliberately not built around

- The viewport is a heuristic where the T# is ground truth. It holds on this workload —
  measured stable per RT base across ~41k traced draws — but a title rendering sub-rects of
  one RT under several viewports would re-key the RT per viewport. A per-base extent
  high-water mark is the fix if a title needs it; not built speculatively.
- The consumer/T# path was considered and rejected: Vulkan cannot crop an image view's
  normalized sampling range, so fixing it there needs UV scaling pushed into every recompiled
  shader or a per-frame sub-region blit. Both are far past the size of this bug.
- `backend.rs::readback` writes `w*h*4` contiguous bytes and is therefore wrong for a padded
  surface. Latent before (accidentally right only while width == pitch), now explicit. Marked
  in a comment; env-gated off and unused by any corpus title. Related to task-181.

### Follow-on

Closing this exposed the next defect: the scene is now full-screen but uniformly over-blurred
against the console reference. Filed as **task-184**.
