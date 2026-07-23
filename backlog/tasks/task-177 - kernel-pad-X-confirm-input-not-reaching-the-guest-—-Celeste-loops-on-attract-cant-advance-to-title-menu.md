---
id: TASK-177
title: >-
  kernel/pad: X/confirm input not reaching the guest — Celeste loops on attract,
  can't advance to title/menu
status: Done
assignee: []
created_date: '2026-07-18 18:29'
updated_date: '2026-07-23 18:41'
labels:
  - kernel
  - pad
  - input
  - celeste
  - retail
dependencies: []
priority: high
ordinal: 181000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Live: maintainer pressed X on the pad but nothing reacted — Celeste stays on the 2D attract/intro screen (CELESTE logo + 2D mountain) and never advances to the interactive title/menu (the scene with the 3D rotating mountain, reached by pressing confirm). Suspected wrong scePad button mapping OR input not being delivered to the guest at all. This BLOCKS reaching the RT-composited scene (task-171/174 validation) and progressing past attract. METHOD: check scePad HLE (crates/libs/src/libscepad or similar) — does scePadReadState/scePadRead deliver button state? Is the host keyboard/controller wired to the guest pad? Verify the CROSS/confirm button bit + the PS4 button bitmask (SCE_PAD_BUTTON_CROSS=0x4000 etc.), analog/touchpad optional. Confirm whether ANY input reaches the guest (trace scePad reads) then fix the mapping so confirm advances attract->menu. Relates task-170 (attract may 'loop' partly because no confirm arrives).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 scePad button state reaches the guest (trace shows the read returning pressed bits when a host key/button is held)
- [ ] #2 The CROSS/confirm button is mapped correctly (PS4 button bitmask) from a host input
- [ ] #3 Pressing confirm advances Celeste from the attract screen to the title/menu — live oracle
<!-- AC:END -->

## Notes

<!-- SECTION:NOTES:BEGIN -->
### Verdict: input is NOT the attract blocker (2026-07-18, opus headless + maintainer live)
Full scePad call-sequence trace (UNEMUPS4_PAD_TRACE=1, 130s run): guest calls `scePadInit()`→0, `scePadOpen(user=1,type=0,index=0)`→handle 16777217, then **NEVER polls** — 0 reads (base+Ext), 0 GetControllerInformation, 0 setter calls, 0 missing-symbol FATAL. So the guest opens the pad but its input-poll loop never runs on the attract screen → pressing Enter/DS4-X can't do anything (no read to land in). **Not pad-gated here; the block is upstream (task-170 advance-gate).**
- Host→guest wiring PROVEN correct: one shared `Arc<RwLock<PadState>>` (main.rs:237/244/277 → bridge.rs:273), CROSS=0x4000 (pad.rs:19), Enter/LCtrl→CROSS (display.rs). Never the problem.
- **Small-handle experiment REFUTED:** forced scePadOpen→1 (real-HW-like) via UNEMUPS4_PAD_SMALL_HANDLE=1 — maintainer tested, no change. Handle value is not why the guest doesn't poll.
- Missing pad SETTERS (scePadSetVibration/LightBar/etc, stubbed-missing at link) are NEVER called (calling one would std::process::exit(1) via lib.rs:92-104; game doesn't crash) → setter-init-fail disproven.

### Real fix built (KEEP — a genuine gap, not the attract blocker) — UNCOMMITTED in worktree agent-aa80d32e202cf6be4
`crates/libs/src/libscepad/mod.rs` (+145/−48): added handlers `scePadReadStateExt`, `scePadReadExt`, `scePadGetControllerInformation` (reports 1 connected DS4); `scePadRead`/`ReadExt` now return the sample `count` (was 0); `UNEMUPS4_PAD_TRACE=1` diagnostic (Init/Open/Close/GetHandle/GetControllerInformation + first read + button-bitmask changes). Build/test/clippy clean (467 pass). NO setter stubs (never called). This is worth landing — Celeste WILL poll the Ext variants once it reaches the menu; but it does not unblock the attract. Confirm/advance host key = Enter (or LCtrl) / DS4 X → CROSS.
<!-- SECTION:NOTES:END -->
