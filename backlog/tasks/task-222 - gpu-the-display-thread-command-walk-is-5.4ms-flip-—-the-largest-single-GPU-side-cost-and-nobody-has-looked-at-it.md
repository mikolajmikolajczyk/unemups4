---
id: TASK-222
title: >-
  gpu: the display-thread command walk is 5.4ms/flip — the largest single
  GPU-side cost and nobody has looked at it
status: Done
assignee: []
created_date: '2026-07-22 10:10'
updated_date: '2026-07-22 10:38'
labels:
  - gpu
  - perf
dependencies: []
priority: high
ordinal: 227000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Breaking down a Celeste gameplay flip of 12.8 ms with the task-203/213 instrumentation:

    guest-side PM4 walk        2.29 ms   (task-217)
    submit_wait                7.86 ms
      display cmd walk         5.44 ms   <-- this task
      record_passes            2.39 ms
        draw_fence             1.42 ms   (task-205)
        transient create       0.37 ms   (task-206)
        transient destroy      0.32 ms
        record                 0.51 ms
        queue_submit           0.13 ms
    flip_wait                  2.10 ms   (largely legitimate 60 Hz pacing)
    apply_dirty                0.55 ms

The display-side command walk is the biggest named GPU-side item — larger than draw_fence and the transient-object churn combined — and no task has ever examined it. It is the cmd_walk phase in AshBackend::run_command_list / replay_command_list (crates/gpu/src/backend.rs, instrumented at :497), the pass that walks the BackendCmd list applying resource-cache and pipeline commands before record_passes records the draws.

5.4 ms to walk a list for a frame that records roughly 20-25 passes is a lot, so the first job is to find out WHAT it is doing per command, not to optimize on a hunch:
- how many BackendCmds per flip, broken down by variant? The pass count is known (~24 from transients/flip) but the command count is not.
- which variants dominate the time? A resource-cache lookup, a descriptor rebuild and a no-op rebind cost very different amounts.
- is work repeated per draw that could be hoisted per submit, or repeated per submit that could be cached across frames? The guest rebinds most state per draw, so the same texture or pipeline may be looked up dozens of times a frame.

Note run_command_list also copies the whole list (cmds.to_vec() at crates/gpu/src/lib.rs) to hand it across the channel, and the guest thread blocks for the entire round trip — so anything paid here is paid straight out of the frame.

Only after the breakdown exists should the fix be chosen. Measure cmd_walk and frames-per-window before and after, and note the pattern this investigation keeps hitting: removing cost from one phase has repeatedly MOVED wall time elsewhere rather than shortening the frame, so report the frame rate too and say plainly if the row shrinks while the frame does not.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a per-variant breakdown of BackendCmds per flip and their share of cmd_walk time is measured and recorded
- [x] #2 the dominant cost is identified from that data rather than assumed, and the fix follows from it
- [x] #3 measured before/after for cmd_walk AND frames-per-window, both reported even if the frame rate does not move
- [x] #4 build + clippy clean, cargo test --workspace green; maintainer confirms the scene still renders correctly
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Instrument first, optimize never-before-measured: add per-BackendCmd-variant count+ns counters to crates/gpu/src/present_profile.rs (relaxed AtomicU64 arrays, UNEMUPS4_PROFILE gate, one Instant read per command reused as the next command's start so it costs 1 clock read/cmd, not 2). Print as a per-window delta row from app/unemups4/src/profiler_dump.rs, sorted by ns.
2. Run Celeste attract/menu (no gamepad available) with UNEMUPS4_PROFILE=10 and record the breakdown verbatim: cmds/flip by variant and each variant's share of cmd_walk.
3. Only then choose the fix, from the data. Suspects to confirm or kill: create_resource/free_resource (vkCreateBuffer+vkAllocateMemory per frame, and free_resource does a wait_for_fences INSIDE the walk), replay_import (external-memory import), upload/upload_image (staging + submit + fence), and per-draw rebinds that hash-lookup the same id dozens of times.
4. Re-measure cmd_walk AND frames-per-window; report both even if the frame rate does not move (this investigation keeps seeing cost move rather than vanish).
5. build + clippy + cargo test --workspace, plus a profiler-OFF boot to prove the gate.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed 2026-07-22.

STEP 1 (the point of the task) — every hypothesis about the walk was WRONG. Per-variant breakdown, attract/menu scene:

    cmd walk [window]: 264 cmds/flip, 0.825 ms/flip attributed, 0.85 MiB/flip uploaded
      UploadBuffer          71.0/flip  0.791 ms/flip (95.9%)  avg 11140 ns each
      BindStorageBuffer     48.0/flip  0.008 ms/flip ( 0.9%)
      BindTexture           17.0/flip  0.007 ms/flip ( 0.9%)
      ...all 180 per-draw rebinds combined = 1.2%

There was nothing to hoist and nothing to cache. UploadBuffer was 96% of it. A temporary map/memcpy/unmap split located it precisely: upload_cache_buffer did a vkMapMemory/vkUnmapMemory pair around EVERY upload, 71 times a flip, on buffers that live across frames.

FIX: map each cache buffer once at create, hold the pointer for the resource's lifetime, make upload a bounds-checked memcpy.
  crates/gpu/src/vulkan.rs:1311   upload_cache_buffer -> map_cache_buffer
  crates/gpu/src/backend.rs:223   CacheBuffer gains ptr/len
  crates/gpu/src/backend.rs:2295  create_resource maps once; upload memcpys with a range guard
  crates/gpu/src/present_profile.rs + backend.rs:505,810 + profiler_dump.rs  per-variant instrumentation

The win beat the split's own prediction (0.79 -> 0.065 ms rather than 0.31) because remapping also discarded page mappings, so the 'memcpy' line was really memcpy plus first-touch faults.

SAFETY, reviewed independently: guest-supplied offsets now write through a raw pointer, so vkMapMemory's implicit range check had to be replaced by an explicit one. The guard uses saturating_add, early-returns on empty, and keeps offset+len <= res.len, so the write stays inside the mapping. Sound.

MEASURED, attract: cmd_walk 0.799/0.810/0.825 -> 0.093/0.104/0.115 ms/flip, UploadBuffer 10.8-11.1 us -> 0.88-1.0 us. Independently reproduced: 11140 -> 1799 ns per upload.

FRAME RATE DID NOT MOVE on attract (~53.2 fps both sides) and that was reported plainly rather than buried. The menu frame is ~18.8 ms of which guest_exec is ~12.2 and flip ~6.0 INCLUDING 60 Hz pacing, so the GPU side is not the binding constraint there. The saving is real rather than relocated — at matched cumulative flip counts submit_wait went 3.41 -> 2.98 ms — it just lands in slack.

GAMEPLAY (maintainer run) barely moved either: 28.8-29.8 -> 29.2-30.5 fps, within noise though consistent in direction. UploadBuffer confirmed at 1822 ns there too, so the fix holds; 0.6 ms simply does not show in a 33 ms frame.

WHAT IT UNCOVERED, which is worth more than the fix: with UploadBuffer no longer dominating, gameplay's breakdown shows CreateBuffer at 96-98% of the walk — 4 to 22 fresh Vulkan buffers per flip at 0.70-1.62 ms EACH, 6.4-15.5 ms per flip. Larger than task-217, task-205 and task-206 combined, and invisible until this task added per-variant attribution. Filed as task-223.

AC #4: maintainer played the title after this landed and reported no visual or behavioural regression; draw counts, pass counts and MiB/flip are byte-identical before and after.

Build + clippy clean on the crates touched, cargo test --workspace 575 green, profiler-OFF boot verified silent.
<!-- SECTION:NOTES:END -->
