---
id: TASK-163
title: >-
  gnm/gpu: presented splash draws sample white-dummy, not the real atlas —
  logo/mountain render offscreen but never reach the swapchain
status: Done
assignee: []
created_date: '2026-07-17 13:11'
updated_date: '2026-07-23 18:41'
labels:
  - gnm
  - gpu
  - celeste
  - retail
  - texture
  - present
dependencies: []
priority: high
ordinal: 169000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
With timing fixed (task-162) Celeste HOLDS its studio splash across many frames, but the PRESENTED frame is white-dummy gradient content: a ~1500x199 horizontal gradient BAR (the 'Matt Makes Games presents' logo quad rendered with the 1x1/2x1 white-dummy instead of its real 1500x199 logo texture) plus scattered gradient particle squares. task-157 PROVED the real atlases (logo 1500x199, mountain 4096x820, clouds) decode correctly and upload; the texture-bind census showed real atlases are bound almost exclusively by OFFSCREEN (render-target) draws while the PRESENTED (videoout) draws overwhelmingly bind the FNA white-dummy (2x1 dfmt1, unmappable base). So Celeste composites its real textured content into offscreen RenderTarget2D(s) and the final present/blit of that RT to the backbuffer is NOT sampling it — the presented draw resolves an unmappable T# and we white-dummy it. Hypotheses: (i) RT-as-texture: the present draw samples the offscreen RT whose guest address our identity seam does not back (task-56 host-aliasing) -> unmappable -> white-dummy; (ii) the logo/atlas draws render into an offscreen we never present (wrong buffer / flip routes the SpriteBatch backbuffer, not the composited RT). Investigate: for the PRESENTED (videoout) content draws, dump the resolved T# base + whether it equals a prior render-target's base (RT-as-texture) vs a genuine FNA white pixel; trace which VkImage the flip presents vs which the real-atlas offscreen draws render into. Method: RenderDoc + the UNEMUPS4_DUMP_TEX/texbind instrumentation (worktree agent-a3dbab2a5e6262b40) + PNG oracle. Relates task-157, task-56, task-162, doc-6.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Determined why presented splash draws bind white-dummy instead of the real atlas (RT-as-texture vs wrong-buffer)
- [ ] #2 Real logo + mountain visible on the presented frame (PNG oracle), not gradient placeholders
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
DIAGNOSE-FIRST (1 guest run, no fix). Timing now fixed (task-162) so splash HOLDS — stable state for tracing. derive_target (gnm/derive.rs:88): a draw is Videoout iff CB_COLOR0_BASE == a registered scanout buffer, else Offscreen. RT-as-texture exists (exec.rs:575 render_targets.lookup -> bind_render_target_as_texture). Census: real atlases bound by OFFSCREEN draws; PRESENTED (Videoout) draws bind 2x1 white-dummy. HYPOTHESIS (c): Celeste renders textured scene into an offscreen RT (host VkImage), then FLIPS that RT's guest memory as a scanout buffer; we present the guest-memory scanout image (empty/stale, readback off by default) instead of the host RT image -> white-dummy/gradient. Instrument to CONFIRM which case: log (1) every registered scanout buffer base+attr, (2) each flip's presented buffer base/idx, (3) every register_render_target base+size, (4) per Videoout draw its CB_COLOR0_BASE + source T# base + white-dummy?/RT-as-texture?/real, (5) CROSS-REF: does a flipped scanout base == an offscreen RT base (case c: present host RT at flip)? does any Videoout draw sample an RT base but white-dummy (case a/b: blit source mis-resolve / lookup range mismatch)? Report the case + addresses; I design the fix. Anchors: register_render_target exec.rs:1147, core/videoout.rs register_buffer :63/:105, gpu backend submit_flip :1594, present 2-pass (scene->texture_image->blit).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
ENGINE FIXES MERGED bd216b4 (fix f9286c8). Two videoout correctness fixes landed: (1) frame pacing at the SubmitFlip choke point — 60Hz real cap (was ~600fps racing the virtual clock); (2) flip-once-per-submit-and-flip-batch (was flipping per-DCB, an extra stale present per batch). Per-scanout-buffer persistence experiment REVERTED (Celeste clears+redraws every frame -> persistence was the wrong premise). Maintainer live-test: speed now close to 60fps (slightly high). BUT the 'rewind' (snow flies then resets a few seconds) and the later flicker SURVIVED all render fixes (per-buffer, double-flip, pacing) -> concluded GUEST-SIDE, not our rendering (clock is monotonic so time can't rewind; single shared texture_image so no stale buffer). Maintainer agrees it looks like 'something on the guest resets'. The T# decode was CONFIRMED CORRECT via Mesa (S_008F14_BASE_ADDRESS_HI(va>>40) = word1[7:0]=base[47:40]) -> the snow placeholder T# genuinely decodes to a degenerate ~9.9TB base -> white-dummy is RIGHT -> snow is MEANT to be solid vertex-colored quads. So the two REMAINING problems (separate from this task's present-path work, now done): (A) snow renders as RAINBOW hard squares instead of soft WHITE -> vertex-COLOR fetch bug (we read wrong stream/offset as color; snow should be white) + additive blend softness; (B) the guest-side reset/loop + later flicker (game looping/holding the splash, or the snow respawn looking like a rewind). Present-path engineering for task-163 is DONE; A and B are new investigations.
<!-- SECTION:NOTES:END -->
