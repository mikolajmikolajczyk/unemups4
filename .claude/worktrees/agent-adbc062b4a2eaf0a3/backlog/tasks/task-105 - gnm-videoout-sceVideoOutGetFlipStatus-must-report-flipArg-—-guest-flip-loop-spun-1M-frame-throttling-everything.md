---
id: TASK-105
title: >-
  gnm/videoout: sceVideoOutGetFlipStatus must report flipArg — guest flip loop
  spun 1M/frame, throttling everything
status: Done
assignee: []
created_date: '2026-07-13 11:11'
updated_date: '2026-07-13 11:28'
labels:
  - real-software
  - doom
  - gnm
  - videoout
  - bug
dependencies: []
ordinal: 104000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Live-debug of Doom (ps4doom): the guest ran but was throttled to ~1fps and input was almost never polled. Root cause: sceVideoOutGetFlipStatus only wrote *ptr=0 (an i32 at offset 0) and never set flipArg. Guest flip loops (the ps4doom shim, and real GNM apps) submit a flip with an arg, then spin on sceVideoOutGetFlipStatus().flipArg (offset 24, i64 in the 64-byte OrbisVideoOutFlipStatus) until it equals the submitted arg. With flipArg left as stack garbage the compare never matched, so the shim spun its full 1,000,000-iteration timeout EVERY frame — each iteration a GetFlipStatus syscall trapping into Rust — pinning DG_DrawFrame at ~1s/frame. That starved DG_PS4_PollInput->scePadReadState (called once per frame, first line of DG_DrawFrame), so keyboard/gamepad input (host capture verified working — set_button fires on both the display and gilrs threads) essentially never reached the guest. FIX: track the last submitted flip arg (AtomicI64 set in sceVideoOutSubmitFlip) and have sceVideoOutGetFlipStatus zero the 64-byte struct and write flipArg at offset 24 = last arg (HLE presents synchronously, so the just-submitted flip is 'done'). The spin now exits on iteration 1, the guest runs at full speed, and per-frame scePadReadState resumes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 sceVideoOutGetFlipStatus writes a zeroed 64-byte OrbisVideoOutFlipStatus with flipArg (offset 24) = last submitted arg; sceVideoOutSubmitFlip records it
- [x] #2 Doom runs at full speed (no 1M-spin per frame); scePadReadState is polled every frame so keyboard+gamepad input reaches the guest (USER-VERIFIED: Escape opens the menu, pad plays)
- [x] #3 6 example baselines still match (softgpu flips too); build/clippy/fmt clean
<!-- AC:END -->
