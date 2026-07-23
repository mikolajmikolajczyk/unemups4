---
id: TASK-192
title: >-
  pad: analog sticks report 0x00 (pinned up) instead of 0x80 center — Celeste
  menu stuck scrolling up
status: Done
assignee: []
created_date: '2026-07-21 09:28'
updated_date: '2026-07-21 10:55'
labels:
  - hle
  - pad
  - celeste
  - input
dependencies: []
priority: high
ordinal: 197000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
PadState sticks default to 0 and nothing ever writes lx/ly/rx/ry, so scePadReadState reports the left stick pinned fully UP every frame (PS4 analog Y: 0x80=center, 0x00=full up). Celeste's menu reads the analog stick, so selection is dragged back to the top item constantly; pressing d-pad down moves it for one frame then the analog-up re-asserts and it snaps back. Root: crates/core/src/pad.rs:22-31 PadState derive(Default) gives lx=ly=rx=ry=0; the host input path crates/gpu/src/gamepad.rs:55-60 DIGITIZES stick axes into PAD_BUTTON_UP/DOWN/LEFT/RIGHT bits and never writes the analog fields; the read handler crates/libs/src/libscepad/mod.rs:64-67 copies the raw 0. Fix: center the sticks at 0x80 (neutral) so an untouched stick reads as centered — via a custom Default for PadState (lx=ly=rx=ry=0x80) or initializing InputManager state to 0x80. Optional follow-up (separate): real analog passthrough (add set_stick, feed gilrs axis values through instead of only digitizing to buttons). Oracle: Celeste menu no longer auto-scrolls to top; down/up navigate normally (maintainer live).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 PadState reports 0x80 (center) for lx/ly/rx/ry when no stick input is present
- [x] #2 Celeste menu selection stays put and navigates up/down normally instead of snapping to the top item (maintainer live oracle)
- [x] #3 build + cargo test + clippy clean
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Manual Default impl for PadState: lx=ly=rx=ry=0x80 (PS4 analog center), buttons=l2=r2=0. Remove derive(Default). Read handlers already copy state raw; set_button only touches buttons, so sticks stay centered when untouched. Passthrough of real axis values = separate follow-up (out of scope).
<!-- SECTION:PLAN:END -->
