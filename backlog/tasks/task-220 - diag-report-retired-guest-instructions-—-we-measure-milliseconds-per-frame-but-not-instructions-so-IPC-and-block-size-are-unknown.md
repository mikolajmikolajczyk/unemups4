---
id: TASK-220
title: >-
  diag: report retired guest instructions — we measure milliseconds per frame
  but not instructions, so IPC and block size are unknown
status: Done
assignee: []
created_date: '2026-07-22 09:36'
updated_date: '2026-07-22 11:42'
labels:
  - diag
  - cpu
  - perf
dependencies:
  - TASK-218
priority: medium
ordinal: 225000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every performance number this project has is time. We know guest execution is about 25 ms of a 40 ms Celeste gameplay frame at 99% on-core, but not how many guest instructions that buys, so two questions that decide where optimization effort goes cannot be answered:

- how far from native are we PER INSTRUCTION, rather than per frame? Celeste holds 60 fps on a 1.6 GHz Jaguar core while we need 25 ms per frame on a far stronger CPU. That comparison suggests a large codegen gap, but without an instruction count it stays an inference from wall clock.
- how long is a compiled unit? chained transfers run about 1000000 per frame; dividing retired instructions by block transitions gives the average basic-block length, which is the number that says whether superblock formation (task-219) has anything to chew on and whether its caps (max_blocks 16, max_icount 256) are the right size.

x86jit already exposes Vcpu::retired_instructions(). The task-218 audit found this repo reads it nowhere.

Add it beside the existing exec counters, using the same shape task-218 established for fast_hits: it is a running per-vcpu total that is not atomic in x86jit, so fold DELTAS from the owning thread — at the frame boundary for the flipping thread, and on the run_guest_call exit path for whatever accrued since. Do NOT re-add the running total; that is the bug task-218 just fixed.

Report per window: retired instructions, instructions per frame, and instructions per block transition (retired / chained). A derived guest MIPS figure is worth printing too — it is the one number that makes the console comparison concrete rather than rhetorical.

Zero cost when UNEMUPS4_PROFILE is unset, like every other counter here.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 retired guest instructions are folded as deltas from the owning thread, covering the main guest thread that never returns from run_guest_call
- [x] #2 per-window rows report retired instructions, instructions per frame, and instructions per block transition (retired / chained)
- [x] #3 a derived guest MIPS figure is printed, so the distance from native hardware is a number rather than an inference from frame time
- [x] #4 zero cost when UNEMUPS4_PROFILE is unset; build + clippy clean, cargo test --workspace green
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Add retired_instructions to ExecStats, folded as DELTAS from the owning thread exactly like fast_hits (task-218): at the frame boundary for the flipping thread, plus the run_guest_call exit path for the remainder. 2. Snapshot it alongside the other exec counters. 3. Print a per-window row: retired instructions, instructions per frame, instructions per block transition (retired/chained), and derived guest MIPS. 4. Verify against a live run that the value advances and the derived figures are sane.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
COMPLETED 2026-07-22 once x86jit task-281 landed the compiled-path instruction count.

WIRING: JitBackend::enable_icount() is called before the backend is boxed (it must precede the first compile — the flag is baked in when a block is emitted) and only when UNEMUPS4_PROFILE is set, since it is not free. crates/cpu/src/guest_vm.rs. Confirmed live: the codegen line reads 'cranelift opt_level=Speed host=Native superblocks=true verifier=false icount=true'. That also settles an alternative explanation offered for the earlier null results — verifier=false means those runs were release builds, not debug.

Vcpu::executed_instructions is folded as deltas from the owning thread, same discipline as fast_hits (task-218).

A SAMPLING BUG I INTRODUCED AND CAUGHT, worth recording because the wrong number was plausible. The first version divided vcpu_executed (folded at the frame boundary) by cache.chained (read live at dump time) and reported 0.4 instructions per block transition — impossible, since a block holds at least one. Two counters, two different sampling instants. Fixed by accumulating the count into FrameStats alongside frames and guest_ns, so every term of the ratio comes from the same boundary. The reason is written at the field so nobody 'simplifies' it back.

MEASURED, Celeste gameplay, three consecutive windows:

    34.44 fps   guest_exec 20.184 ms/frame   2.92 M instr/frame   145 MIPS
    38.06 fps   guest_exec 20.308 ms/frame   2.63 M instr/frame   129 MIPS
    50.21 fps   guest_exec  9.180 ms/frame   1.23 M instr/frame   133 MIPS

Stable at 129-145 MIPS. Menu windows are not comparable and should not be quoted — an idle menu reads 65 MIPS and the boot window 3073, the latter being Mono initialisation in tight loops.

THE ANSWER THIS TASK EXISTED FOR: the guest is NOT doing an abnormal amount of work. 2.6-2.9 M instructions per frame at 60 fps is about 175 M instr/s, which the real 1.6 GHz Jaguar handles while holding 60 fps. The instruction count is ordinary; we simply execute each one in about 7 ns, roughly 30 host cycles. So the gap is EXECUTION SPEED PER INSTRUCTION, which places it in the lift rather than in Cranelift's mid-end — consistent with opt_level=Speed and the IBTC probe both measuring as no change, and superblocks giving only 5-8%.

Filed as x86jit task-282.

CAVEAT kept explicit: executed_instructions counts compiled code. If a meaningful share still runs interpreted the true count is higher and the MIPS figure understated. The hits:misses ratio of about 200:1 says hot code is compiled, so the direction holds, but the absolute value is a floor.

AC #1 was already ticked when the interpreter-only counter was wired; ACs #2-#4 are now met. Build clean, cargo test --workspace 576 green.
<!-- SECTION:NOTES:END -->
