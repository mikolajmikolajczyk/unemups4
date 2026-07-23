---
id: TASK-215
title: >-
  diag: is the frame thread computing or descheduled? wall-vs-CPU time first,
  then guest RIP sampling
status: Done
assignee: []
created_date: '2026-07-21 21:57'
updated_date: '2026-07-22 05:51'
labels:
  - diag
  - perf
  - cpu
dependencies:
  - TASK-213
priority: high
ordinal: 220000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-213 bounded the judder; it cannot name it. Gameplay measured over four windows with that instrument:

    frame avg 40.6 -> 45.0 ms = guest_exec 26.5-27.5 + flip 13.3-17.0 + other_syscalls 0.38 + run_loop 0.40
    p50 36.7-40.9 | p95 60-69 | p99 73-82 | max 238 ms
    22-25 fps

Guest x86 execution is about 62% of the frame and is now the single largest cost, ahead of the whole GPU submit path. Slow frames balloon in guest_exec (34-42 ms) and flip (26-32 ms) together, while other_syscalls stays at 0.5 ms.

Everything cheap is already ruled out. Blocking HLE calls: the most frequent longest-syscall-in-a-slow-frame is sceGnmSetVsShader at an average of 0.010 ms, i.e. nothing blocks and it is merely the largest of many tiny calls; only sem_wait (16.8 ms) is a real block and it tops just 11 slow frames. MAILBOX discards: 0 of 223 presents in a window landed inside one 60 Hz period. JIT recompilation: compile_ns flat.

What is missing is attribution INSIDE the guest. Drive the existing UNEMUPS4_EXECTRACE backtrace sampler off the slow-frame predicate rather than a wall-clock timer: when a frame crosses the task-213 slow threshold, sample the flipping thread's guest RIP (and, where the loader's module map allows, resolve it to a module plus offset or symbol). Aggregate across slow frames so a repeated address rises to the top.

That is what distinguishes a Mono garbage collection from ordinary game logic, and the two are indistinguishable in every measurement taken so far. Note the judder is NOT periodic: hitch gaps average 7-11 frames with bursts of 1.2 frames, so it is broad-spectrum roughness rather than a cyclic pause — evidence that already argues against a simple periodic GC, which makes naming the code all the more necessary.

Do not conclude from a plausible story. The output must be an address or a symbol backed by counts.

DO THIS FIRST, it may invalidate the rest of the task. `guest_exec` is measured with `Instant::now()` (crates/cpu/src/exec.rs:341-351) — it is WALL time inside cpu.run(), not CPU time. So "26 ms of guest_exec" does not mean the guest executed 26 ms of work; it means 26 ms elapsed while the thread was inside cpu.run(), INCLUDING any time the host descheduled it. Sampling a RIP assumes the thread is running. If it is preempted, the samples are noise.

So the first step is to compare, for the flipping thread, wall time against thread CPU time (CLOCK_THREAD_CPUTIME_ID, one read per frame boundary):
- guest_exec approximately equals CPU time -> the guest really is computing, and RIP sampling is the right next move
- guest_exec much greater than CPU time -> the thread is blocked or preempted, and the question changes entirely from "which guest code" to "what is descheduling us"

The maintainer raised host-side lock contention as a candidate and it survives the evidence so far. crates/kernel/src/sync.rs:202 calls cond.notify_all() on every final mutex_unlock — a thundering herd, and the profiler shows scePthreadMutexUnlock at 1937972 calls averaging 567 ns against scePthreadMutexLock at 1711990 calls averaging 244 ns. Unlock costing 2.3x lock is backwards for an operation that should be trivial, and is the signature of waking every waiter. That cost lands on OTHER threads and on the host scheduler, so it would not appear in the frame thread's other_syscalls (0.5 ms in slow frames) — which is exactly why the earlier reading of that number as exonerating did not hold.

Note also SyncManager keeps mutexes/condvars in process-wide RwLock<HashMap>s that every one of those ~3.6M lock/unlock calls traverses.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 the guest RIP of the flipping thread is sampled when a frame crosses the slow threshold, not on a wall-clock timer
- [x] #2 samples are aggregated across slow frames and reported ranked, resolved to module plus offset or symbol where the loader map permits
- [x] #3 the report names the dominant guest code in slow frames, or states explicitly what prevented resolution
- [x] #4 zero cost when the profiler is off; build + clippy clean, cargo test --workspace green
- [x] #5 wall time vs thread CPU time (CLOCK_THREAD_CPUTIME_ID) is measured for the flipping thread and reported per window, so computing and descheduled are distinguishable
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. crates/cpu/src/profile.rs: per-frame thread CPU time (CLOCK_THREAD_CPUTIME_ID) + context-switch counts (getrusage RUSAGE_THREAD) sampled at the flip boundary and at flip-dispatch entry, so frame wall/CPU and the flip's own wall/CPU are separable — the non-flip remainder is guest_exec to within ~3%. Extend FrameStats/SlowStats with cpu_ns, flip_cpu_ns, vcsw, ivcsw.
2. crates/cpu/src/exec.rs: call a new profile::frame_syscall_enter(id) before dispatch (profiling gate only) to take the flip-entry CPU reading.
3. Step 2, only if step 1 says the thread is computing: opt-in UNEMUPS4_PROFILE_RIP block budget so Exit::BudgetExhausted yields guest RIP; per-thread fixed ring of recent samples, committed into a global fixed-slot aggregate ONLY when the frame closes slow — i.e. driven by the task-213 predicate, not a wall clock.
4. app/unemups4/src/profiler_dump.rs: new rows (wall vs CPU per window and per slow frame, ctx switches/frame, ranked slow-frame RIPs).
5. app/unemups4/src/main.rs: install a guest-address resolver backed by ModuleManager::nearest_symbol so RIPs print as module!symbol+off.
6. Verify: build, clippy, cargo test --workspace, profiler-OFF boot, and a real attract-scene run with the rows quoted verbatim.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Instrumentation only; no fix. Scene measured: ATTRACT/TITLE/MENU (no gamepad available), never gameplay.

STEP 1 (AC #5) — the discriminator. crates/cpu/src/profile.rs samples CLOCK_THREAD_CPUTIME_ID at the flip boundary and at flip-dispatch entry, plus getrusage(RUSAGE_THREAD) at the boundary: 3 clock reads/frame, ~1.3 us, vs a 17-40 ms frame. Result, menu scene, slow frames whose profile matches gameplay (guest_exec-dominant, other_syscalls 0.33 ms):

  slow frame avg 29.910 ms = guest_exec 22.989 + flip 6.281 + other_syscalls 0.326 + run_loop 0.314
  slow frame wall vs thread cpu: cpu 25.264 of 29.910 ms (84% on-core) - flip cpu 1.858 of 6.281 ms (30%); rest of the frame cpu 23.406 of 23.629 ms (99%)
  slow frame context switches/frame: 2.0 voluntary + 2.0 involuntary

guest_exec wall == guest_exec CPU to within 1%. The frame thread is COMPUTING, not descheduled. Step 2 licensed.

The lock-contention hypothesis is REFUTED for this scene: a thread that spends 99% of its guest-execution wall on-core and blocks twice per frame cannot be losing time to notify_all thundering herds or SyncManager RwLock traversal. Unlock's 567 ns average is real but it is not stealing the frame thread's time. Two caveats: attract != gameplay, and the instrument measures only the frame thread, so wasted CPU on OTHER threads is untested.

Attract also has a second, unrelated hitch class (other_syscalls 24 ms, 20% on-core, sem_wait) that task-213 already attributes; that one is a genuine block, not descheduling.

STEP 2 (AC #1-#3) — UNEMUPS4_PROFILE_RIP (opt-in on top of UNEMUPS4_PROFILE so the block budget cannot perturb the frame budget printed beside it). Samples buffer per-thread and are committed to a global table only when the frame closes slow. Menu scene, 17726 samples over 48 slow frames:

  0x1c1f161 496 (2.8%) / 0x1c1f17c 451 (2.5%) / 0x1c1f140 425 (2.4%) / 0x1c1f173 298 (1.7%) / 0x1c1f168 269 (1.5%)  -> eboot.bin +0x297140..+0x29717c, ONE 0x3c-byte loop, ~10.9% of all slow-frame samples
  second cluster eboot.bin +0x64113c..+0x641165 (~5.7%), third +0x66e631..+0x66e65c (~2.3%)

eboot.bin exports 0 symbols, so resolution stops at module+offset (AC #3's escape clause). No single dominant address: the top 12 sum to ~19%, i.e. broad guest work with a few hot loops - consistent with task-213's finding that the judder is broad-spectrum, and NOT the signature of a periodic GC pause.

NEXT (needs the maintainer + a gamepad): rerun in gameplay with
  UNEMUPS4_PROFILE=10 UNEMUPS4_PROFILE_RIP=1 RUST_LOG='error,unemups4::profile=info'
and read the 'slow frame wall vs thread cpu' row. If 'rest of the frame' is again ~99% on-core, the 26 ms guest_exec is real computation and the fix is guest-code throughput (JIT quality / the +0x297140 loop), not scheduling.

Files: crates/cpu/src/profile.rs (sched counters, RIP sampler, describe_guest_addr), crates/cpu/src/exec.rs (frame_syscall_enter hook, rip budget, sample on BudgetExhausted, fault_context pub(crate)), app/unemups4/src/profiler_dump.rs (scheduler_rows, slow_frame_rips).

Verified: cargo build --release clean; clippy -p ps4-cpu -p unemups4 --all-targets --all-features -D warnings clean (gcn.rs's 4 pre-existing errors untouched); cargo test --workspace green; profiler-OFF run boots to asset loading and flips with zero profiler output.
<!-- SECTION:NOTES:END -->
