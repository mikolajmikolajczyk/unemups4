---
id: TASK-17
title: >-
  softgpu perf: async flip handshake — signal guest after staging copy, not
  after present+sleep
status: Done
assignee: []
created_date: '2026-07-10 09:28'
updated_date: '2026-07-10 13:46'
labels:
  - perf
  - gpu
dependencies: []
priority: high
ordinal: 17000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Frame pipeline is fully serial: GpuManager::submit_flip (crates/gpu/src/lib.rs:22-27) blocks the guest on rx.recv(), and the display loop sends the signal only at display.rs:203 — after fence wait, acquire, 8.3MB staging memcpy, submit, present AND the pacing sleep (:190-194). Chain = guest draw + whole present path + sleep, sequentially; with FIFO present (vulkan.rs:294) any chain >16.6ms quantizes to ~2 vsync periods = the observed ~34 fps. Fix: send the vsync signal immediately after the guest framebuffer is copied to staging (display.rs:146) — the submitted buffer is consumed at that point and the guest double-buffers, so it can safely draw the next frame in parallel with GPU submit/present. This approximates real PS4 flip-queue semantics (flip queued, not flip displayed). Keep pending_vsync_signal replacement logic in AboutToWait (:82-84) intact for the queue-collapse case.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Signal sent right after the staging copy; present path and pacing sleep no longer gate the guest
- [x] #2 ps4-softgpu fps improves measurably (target: vsync-limited 60 or draw-limited, not serial-chain-limited); number recorded in task notes
- [x] #3 No regression in other examples (scripts/run_examples.sh green); no tearing/artifacts in softgpu visual check (double buffering still correct)
- [x] #4 Comment in display.rs documents the flip-queued vs flip-displayed semantics choice
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Move the vsync signal from after present+pacing-sleep (display.rs:202-204) to right after the staging memcpy completes (after :145). Rationale: once the guest framebuffer bytes are copied into the staging buffer, that buffer is consumed and the guest (which double-buffers) can safely draw the next frame in parallel with GPU submit/present/pacing -- flip-QUEUED not flip-DISPLAYED semantics (approximates PS4 flip-queue).

Correctness:
- Signal AFTER the copy (copy completes before signal -> no tearing into the buffer the copy reads). This is the whole point.
- All RedrawRequested exit paths must still fire the signal exactly once so the guest never deadlocks on rx.recv(): the window-size==0 early return already sends it; the acquire OUT_OF_DATE / error early returns currently 'return' WITHOUT sending -> must send before those returns too (today they leak the signal until the next frame replaces it; keep behavior safe).
- Exactly one signal per SubmitFlip: take() the pending signal right after the copy and send it; downstream present/sleep no longer touch it.
- Keep AboutToWait :82-84 replace()/collapse logic intact.
- AC#4: comment documenting flip-queued vs flip-displayed choice.

Verify: build/clippy/test/fmt green; oracle green (only env line); live FPS handed to maintainer (headless can't init Vulkan).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-10.

DONE:
- AC#1: the vsync signal (pending_vsync_signal.take() -> send) is now sent in RedrawRequested immediately after the guest-framebuffer->staging memcpy completes (crates/gpu/src/display.rs, right after the copy_nonoverlapping block), instead of after submit/present/pacing at the end of the frame. Removed the old end-of-frame send.
- AC#4: added a comment documenting the flip-QUEUED (queued, not yet on-screen) vs flip-DISPLAYED semantics and why parallelism is safe.

DESIGN / CORRECTNESS:
- The memcpy is a synchronous CPU copy that fully completes before the signal, so the guest resumes only after its framebuffer bytes are already in the Vulkan staging buffer. The subsequent cmd_copy_buffer_to_image reads from staging (not guest memory), so even if the guest immediately overwrites its framebuffer there is no tearing into the bytes being uploaded.
- present + submit + vsync-pacing (FRAME_DURATION sleep) now run in parallel with the guest's next frame. This is what stops a >16.6ms serial chain (guest draw + full present + sleep) from quantizing to ~2 FIFO vsync periods (~34 fps).
- Exactly one signal per SubmitFlip: taken once after the copy. AboutToWait's pending_vsync_signal.replace()/collapse path (display.rs:82-84) is untouched and still handles a new flip arriving before the previous signal drains.
- Early-return paths: window-size==0 path still signals before returning (unchanged). The acquire OUT_OF_DATE/error early-returns occur BEFORE the copy and do not signal that frame -- identical to prior behavior (the pending signal was previously only sent at end-of-frame, which those returns also skipped); the pending signal is collapsed by the next SubmitFlip. No new deadlock.

VERIFICATION:
- cargo build --release -p ps4-gpu green; clippy --all-targets --all-features -D warnings clean; cargo test green (run_guest 9, loader 3, vm_backend 7); fmt clean.
- Oracle run_examples.sh check: only the known headless env Vulkan line diverges; no guest-output regression. (Display examples never reach the flip loop headless -- no Vulkan driver -- so this change is compile+reasoning-verified here, not exercised.)

AC#2 (fps improves) + AC#3 (no tearing/artifacts visual check) -- HANDED TO MAINTAINER: require a live Vulkan run. Maintainer to run ps4-softgpu under UNEMUPS4_BACKEND=jit on a machine with a Vulkan driver, before/after this commit; expect fps to jump from serial-chain-limited ~34 toward vsync-limited 60 (or draw-limited), and confirm no tearing/artifacts (double-buffering + memcpy-before-signal preserves correctness). Record the number.

Status left In Progress for maintainer.

2026-07-10 (maintainer live verification): ps4-softgpu under UNEMUPS4_BACKEND=jit on a real Vulkan driver, combined with task-16: **34 fps → 60 fps (vsync cap)**, fully smooth, **no tearing/artifacts**. AC#2+#3 ticked; merged to main; Done.
<!-- SECTION:NOTES:END -->
