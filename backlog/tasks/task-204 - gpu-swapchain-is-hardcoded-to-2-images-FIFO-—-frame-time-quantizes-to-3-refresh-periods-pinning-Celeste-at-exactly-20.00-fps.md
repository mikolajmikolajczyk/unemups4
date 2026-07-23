---
id: TASK-204
title: >-
  gpu: swapchain is hardcoded to 2 images + FIFO — frame time quantizes to 3
  refresh periods, pinning Celeste at exactly 20.00 fps
status: Done
assignee: []
created_date: '2026-07-21 18:28'
updated_date: '2026-07-21 22:01'
labels:
  - gpu
  - perf
  - vulkan
dependencies: []
priority: high
ordinal: 209000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/gpu/src/vulkan.rs:557 sets min_image_count: 2 and :570 sets PresentModeKHR::FIFO, both hardcoded, ignoring the surface capabilities the same file already queries into surface_min_image_count / surface_max_image_count (vulkan.rs:429-444) and ignoring the modes the device actually supports.

Double-buffered FIFO means any frame whose work exceeds one refresh period stalls in acquire_next_image until the display releases an image, so the achieved rate collapses onto an integer division of the refresh rate. Measured on Celeste: 200 frames per 10.000 s, in every single 10 s window — 20.00 fps dead flat, which is exactly 3 periods of the 59.95 Hz output. The flatness IS the evidence: a load-bound frame rate wobbles, a quantized one does not. The display-thread counter agrees — acquire alone blocks 12.9 ms/frame while pace_sleep is 0.000, i.e. the frame limiter never engages because we never run ahead.

Change:
- request surface_min_image_count + 1 (clamped to surface_max_image_count when non-zero) instead of the constant 2
- select the present mode from what the surface reports rather than assuming FIFO; FIFO is the only universally-guaranteed mode so it stays the fallback
- keep the existing swapchain-recreate path working

This does NOT make a slow frame fast — it removes the quantization, so the frame rate becomes the real work rate instead of the next integer division below it. Expect roughly a doubling on the current workload; the actual per-frame cost is task-203/204's problem.

Verify by measurement, not by reasoning: rerun with UNEMUPS4_PROFILE and compare frames-per-interval and the acquire average before/after. Note the maintainer's eyes remain the visual oracle — confirm no tearing or presentation regression on screen.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 swapchain image count derives from the surface capabilities (min + 1, clamped to max) instead of the hardcoded 2
- [x] #2 present mode is chosen from the modes the surface reports, with FIFO as the fallback
- [x] #3 measured: frames-per-10s is no longer pinned to an exact integer division of the refresh rate, and the acquire per-frame average drops; before/after numbers recorded in the task notes
- [x] #4 maintainer confirms on screen that presentation is visually unchanged (no tearing, no stutter regression); build + clippy clean
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. crates/gpu/src/vulkan.rs create_swapchain: derive min_image_count from the already-queried surface capabilities (caps.min_image_count + 1, clamped to caps.max_image_count when non-zero) instead of the hardcoded 2.
2. Query get_physical_device_surface_present_modes and pick from what the surface reports — MAILBOX first (tear-free and non-blocking, so acquire stops waiting on the display), FIFO as the universally-guaranteed fallback. Log the pick.
3. No swapchain-recreate path exists today (present() returns on ERROR_OUT_OF_DATE_KHR) and the framebuffers/image views are built per swapchain image, so a larger image count needs no other change.
4. Measure with UNEMUPS4_PROFILE=10 on Celeste: frames per 10 s window and the acquire per-frame average, before vs after, as window deltas rather than running averages.
5. Flag for the maintainer's eyes whether presentation is visually unchanged.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
CHANGE: crates/gpu/src/vulkan.rs create_swapchain — min_image_count is now caps.min_image_count + 1 (clamped to caps.max_image_count when non-zero) and the present mode is picked from get_physical_device_surface_present_modes: MAILBOX when offered, FIFO otherwise. MAILBOX rather than IMMEDIATE deliberately — it stops acquire blocking on the present queue but still scans out on vblank, so it cannot tear. No swapchain-recreate path exists to keep working (present() just returns on ERROR_OUT_OF_DATE_KHR) and the image views + framebuffers are already built per swapchain image, so nothing else needed changing.

WHAT THE SURFACE ACTUALLY REPORTS (this box, Wayland): min 3, max 0 (unlimited). So the old hardcoded 2 was BELOW the surface minimum — an invalid VkSwapchainCreateInfoKHR the driver silently clamped to 3. The real change is 3 -> 4 images AND FIFO -> MAILBOX. Logged at debug: 'swapchain: 4 images (surface min 3 max 0), present mode MAILBOX'.

MEASURED (Celeste gameplay, UNEMUPS4_PROFILE=10, per-10 s-window deltas, not running averages):

                            BEFORE (2/FIFO)      AFTER (4/MAILBOX)
  frames per 10 s window    200, 200, 200 ...    229, 230, 231, 232 (varies)
  fps                       20.00 dead flat      22.9 - 23.2
  frame time                50.0 ms              43.7 ms
  acquire_next_image        13.060 ms/frame      0.075 ms/frame
  guest flip round trip     13.137 ms/flip       0.121 ms/flip
  flip syscall              36.172 ms/call       28.460 ms/call

AC #3 is met on both halves: the rate is no longer an exact integer division of the 59.95 Hz output (59.95/2 = 29.98, /3 = 19.98; 23.1 is neither) and it now wobbles window to window the way a load-bound rate does, and acquire fell by 174x.

BUT THE GAIN IS +15%, NOT THE PREDICTED DOUBLING, and the reason is worth recording. Removing the 13.06 ms acquire stall only bought 6.3 ms of frame time, because the guest-thread PM4 decode measured by task-203 got SLOWER once the guest stopped being blocked: decode 11.65 -> 15.21 ms and packet free 4.92 -> 6.81 ms, +5.45 ms/flip on an identical ~525k packets. Consistent across every steady-state window in both runs, so it is a real effect, not noise — the guest thread now runs continuously and contends with the display thread for memory bandwidth and the allocator instead of idling inside acquire. The frame is now 78% PM4 decode + packet-vector free (22.0 of 28.5 ms). That is where the next win is (the ~4 MB NOP dcb[0] decoded into 525k heap-allocated packets every single frame), not in the swapchain.

VERIFIED: cargo build --release clean; cargo clippy --all-targets --all-features clean for this change (4 pre-existing errors remain in crates/gnm/src/shader/gcn.rs test asserts, an unmodified file); cargo test --workspace green; cargo fmt clean for every file touched.

AC #4 LEFT UNTICKED — it needs the maintainer's eyes. MAILBOX cannot tear by construction (it still presents on vblank) but it DOES discard an already-queued image when a newer one arrives, so a visual check for stutter/judder on screen is the deciding oracle here, and I cannot be that oracle.
<!-- SECTION:NOTES:END -->
