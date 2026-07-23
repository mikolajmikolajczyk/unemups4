---
id: TASK-203
title: >-
  gpu/diag: instrument the GNM submit path — run_command_list/record_passes are
  ~25ms of an unmeasured 50ms frame
status: Done
assignee: []
created_date: '2026-07-21 18:27'
updated_date: '2026-07-21 22:01'
labels:
  - gpu
  - perf
  - diag
  - dx
dependencies: []
priority: high
ordinal: 208000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Celeste steady state measures exactly 200 frames per 10.000s (20.00 fps). The aggregate profiler (UNEMUPS4_PROFILE) attributes 38.4 ms/frame to sceGnmSubmitAndFlipCommandBuffers, of which only 13.7 ms is accounted for by the existing present-phase counters (acquire 12.9, fb_copy 0.47, record_submit 0.24, present 0.06). The remaining ~24.7 ms is arithmetic, not measurement: AshBackend::run_command_list and record_passes carry ZERO instrumentation, so the single largest slice of the frame is invisible.

Close the gap so the gpu present table sums to the flip syscall's own total:
- time run_command_list end-to-end (guest-thread side, including the channel round trip to the display thread), separately from the record work itself
- inside record_passes: time spent recording, time spent in the per-submit wait_for_fences, and the time creating/destroying the transient VkRenderPass / VkFramebuffer / VkDescriptorPool objects
- count per submit: passes recorded, transient render passes, transient framebuffers, descriptor pools, draws
- add matching tracing spans so the same breakdown shows up as nested zones under the existing frame zone in Tracy (--features profile-tracy)

Follow the house pattern of crates/gpu present_profile: relaxed AtomicU64 behind the already-resolved enabled() gate, zero cost when UNEMUPS4_PROFILE is unset, printed by app/unemups4/src/profiler_dump.rs.

AC verification is arithmetic: after this lands, the printed per-frame GPU rows plus the new rows must account for the sceGnmSubmitAndFlipCommandBuffers average within a few percent, instead of leaving half the frame unexplained.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 run_command_list and record_passes emit per-frame timings (record, fence wait, transient object churn) through the existing UNEMUPS4_PROFILE gate, zero cost when unset
- [x] #2 the profiler dump's GPU rows plus the new rows account for the sceGnmSubmitAndFlipCommandBuffers per-call average within a few percent — no unexplained multi-ms remainder
- [x] #3 per-submit counts (passes, transient render passes/framebuffers/descriptor pools, draws) are reported
- [ ] #4 the same breakdown appears as nested Tracy zones under the frame zone; build + clippy clean, cargo test --workspace green
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Add SubmitStats to crates/gpu/src/present_profile.rs (relaxed AtomicU64, same enabled() gate as PresentStats): guest-side round-trip ns for run_command_list/submit_flip, display-side backend total, cmd-walk, record_passes total, pass-record, transient create/destroy, queue_submit, per-submit fence wait, readback; plus counts (submits, passes, draws, transient rp/fb, descriptor pools).
2. Instrument GpuManager::run_command_list + GpuManager::submit_flip (crates/gpu/src/lib.rs) for the guest-thread channel round trip.
3. Instrument AshBackend::run_command_list (crates/gpu/src/backend.rs) — walk vs record_passes vs readback — and record_passes' internals (record loop, transient create, queue_submit, wait_for_fences, transient destroy); thread the prof gate into record_pass_into for the descriptor-pool create.
4. Add matching tracing::debug_span!s so the breakdown shows as nested Tracy zones.
5. Print the new rows in app/unemups4/src/profiler_dump.rs, per frame, plus the derived residual vs the sceGnmSubmitAndFlipCommandBuffers per-call average so the arithmetic AC is checkable from the dump itself.
6. Measure on Celeste (UNEMUPS4_PROFILE=10) and close any remaining multi-ms residual.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Celeste steady state (gameplay, 10 s window 160-170 s, 200 flips / 200 frames = 20.00 fps flat). Every number below is a per-window DELTA of the cumulative counters, not the printed running average.

FLIP BUDGET, fully closed — sceGnmSubmitAndFlipCommandBuffers avg 36.172 ms/call:
  gnm handler (record_submit)                36.165   (99.98% — syscall overhead 0.007 ms)
    pm4 decode_submit_range                  11.652   525,136 packets/flip
    freeing the packet vector                 4.919   one heap block per packet
    packet walk                               0.746
    apply_dirty (drain + caches)              0.062
    guest_submit_wait (display round trip)    5.637
      display backend (AshBackend::run_command_list) 5.605
        cmd walk (pipeline/resource cmds)     0.698
        record_passes                         4.901
          record loop                         0.434  (transient_create 0.306 inside)
          queue_submit                        0.088
          wait_for_fences (per submit)        3.785
          transient destroy                   0.605
        readback                              0.001
    guest_flip_wait (present round trip)     13.137
      of which acquire_next_image            13.060
Counts per flip: 1 command list, 21.4 passes = 21.4 draws, 20.4 transient VkRenderPass, 20.4 VkFramebuffer, 21.4 VkDescriptorPool.

WHAT THE MEASUREMENT OVERTURNED. The task assumed the ~25 ms hole was run_command_list/record_passes. It is not: the display-thread record path is only 5.6 ms. The hole is guest-thread PM4 work — 16.6 ms of decode_submit_range + freeing its Vec<OwnedPacket>, because Celeste's dcb[0] is ~4 MB of NOP padding that decodes to ~525k packets EVERY FRAME, each an OwnedPacket with its own heap-allocated body. That is 46% of the flip. The packet free (4.9 ms) was invisible even to Executor::run's own timer because the vector dropped at scope end, after the timing; it is now dropped explicitly inside the timed region.

CHANGES
  crates/gpu/src/present_profile.rs      +SubmitStats/SubmitSnapshot/submit_snapshot (17 counters)
  crates/gpu/src/lib.rs:64,140           guest-thread round-trip timing for submit_flip / run_command_list
  crates/gpu/src/backend.rs:437          run_command_list wrapper (backend_ns) + replay_command_list
  crates/gpu/src/backend.rs:497,847      cmd-walk and readback timing
  crates/gpu/src/backend.rs:1250         record_passes: record / transient create+destroy / queue_submit / draw_fence + counts
  crates/gpu/src/backend.rs:3160         record_pass_into returns its descriptor-pool creation ns
  crates/gnm/src/profile.rs              NEW — same env gate, guest-thread submit counters
  crates/gnm/src/exec.rs:167             Executor::run: run/decode/packet_free/packets + explicit timed drop
  crates/libs/src/libscegnmdriver/submit.rs:194  record_submit: handler total, driver-lock wait, apply_dirty
  app/unemups4/src/profiler_dump.rs      prints the gpu submit / pm4 exec / flip budget rows

VERIFIED: cargo build --release clean; cargo test --workspace green; clippy clean for every file this task touched (4 pre-existing errors remain in crates/gnm/src/shader/gcn.rs test asserts, an unmodified file).

AC #4 LEFT UNTICKED, deliberately. tracing spans are in place and nest correctly among themselves (gnm_record_submit > apply_dirty, pm4_exec > pm4_decode, gpu_submit_wait/gpu_flip_wait, gnm_submit > cmd_walk / record_passes > record, queue_submit, draw_fence, transient_destroy), but they are NOT children of the existing 'frame' zone: the submit work runs in winit's AboutToWait callback and 'frame' is entered in RedrawRequested, so on the Tracy timeline they are siblings on the display thread, not nested. Making them nest needs the display loop restructured, which is outside this task. Nor were they eyeballed in a live Tracy GUI.

FOLLOW-UPS this measurement justifies: task-205 (per-submit wait_for_fences, 3.8 ms), task-206 (20 transient render passes+framebuffers and 21 descriptor pools created and destroyed per flip), and a NEW one for the real headline — skip decoding the ~4 MB NOP DCB instead of allocating 525k packets for it (16.6 ms/flip).

CLOSED 2026-07-22 with AC #4 STILL UNTICKED. The instrumentation this task exists for landed and did its job — it is what redirected the work from the display thread to guest-side PM4 decode, producing task-208 and the 20 -> ~58 fps result (commit <prior-history>). The Tracy half of AC #4 is genuinely not met and is not being quietly counted as done:

- the zones are siblings of 'frame', not children, for the structural reason above; fixing that means restructuring the winit display loop
- the spans have since been seen to work end-to-end only insofar as the Tracy GUI itself now launches (it needed __EGL_VENDOR_LIBRARY_DIRS=/usr/share/glvnd/egl_vendor.d, LIBGL_DRIVERS_PATH=/usr/lib/dri and LD_LIBRARY_PATH=/usr/lib on this non-NixOS host, because the nix-built GUI's libglvnd cannot find the system EGL vendors). Nobody has yet inspected a live timeline.

If Tracy becomes the primary instrument for a future investigation, the nesting is worth its own task; the text profiler covered every question asked so far.
<!-- SECTION:NOTES:END -->
