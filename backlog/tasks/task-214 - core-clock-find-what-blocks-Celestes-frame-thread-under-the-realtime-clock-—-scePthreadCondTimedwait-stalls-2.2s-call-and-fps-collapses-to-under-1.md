---
id: TASK-214
title: >-
  core/clock: find what blocks Celeste's frame thread under the realtime clock —
  scePthreadCondTimedwait stalls 2.2s/call and fps collapses to under 1
status: Done
assignee: []
created_date: '2026-07-21 20:40'
updated_date: '2026-07-21 22:01'
labels:
  - core
  - clock
  - diag
dependencies:
  - TASK-211
priority: high
ordinal: 219000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Blocks decision-8. task-211 built the realtime time base, but it cannot become the default until this is understood.

A/B on one binary, one scene (Celeste attract/menu), UNEMUPS4_CLOCK the only difference:

    window   realtime      fixed-step
    1        52.79 fps     58.79 fps
    2        10.08 fps     58.19 fps
    3         0.55 fps     54.62 fps
    4         0.46 fps     —

realtime collapses within ~30 s and never recovers; fixed-step holds ~58 fps over the same windows. So the mode switch itself causes it.

What the collapse looks like, which is the useful clue: the frame thread is BLOCKED, not spinning. guest_exec stays at 4.5-8 ms/frame while other_syscalls climbs 83 -> 2177 ms/frame. scePthreadCondTimedwait averages 2176.9 ms per call, matching the stalled frame almost exactly. The guest is waiting for a wakeup that does not arrive.

Already ruled out, do not re-derive:
- the timeout conversion is NOT the bug. The guest passes relative microseconds straight through (crates/libs/src/libkernel/pthread.rs sce_pthread_cond_timedwait) and crates/kernel/src/sync.rs cond_timedwait waits on real host time via wait_timeout. The virtual clock is not in that path.
- a fixed-timestep catch-up spiral does NOT fit: that burns CPU, and guest_exec is low throughout.

Find:
- which condvar the frame thread waits on, and which guest thread is supposed to signal it
- what that signaller is itself waiting for, and how the time base changes its behaviour
- whether the wakeup depends on a guest-visible clock read anywhere in the chain

Candidates worth checking early, from the same profiler run: sceAudioOutOutputs (top syscall by total time, ~18-19 ms/call), sceKernelWaitSema (~18 ms/call), sceKernelUsleep (~8 ms/call, ~10k calls/min). Audio is paced in real time while, under fixed-step, the world is not — decision-8 predicted that relationship would change and this may be where it bites.

Method note: this is exactly the kind of question the retail bring-up method (doc-4) exists for, and the guest-frame instrumentation from task-209 is already in place. Prefer a measurement that names the waiting thread and its signaller over reasoning about what Mono probably does.

Update backlog/decisions/decision-8 with the mechanism once found — that record currently states the premise was refuted but not why.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 the condvar the frame thread blocks on is identified, along with the thread expected to signal it and why it does not
- [x] #2 the causal chain from the realtime time base to the missing wakeup is demonstrated by measurement, not argued
- [x] #3 either realtime runs without collapse and can become the default, or the record states precisely what would have to change for it to
- [x] #4 decision-8 updated with the mechanism; build + clippy clean, cargo test --workspace green
<!-- AC:END -->



## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
ROOT CAUSE FOUND 2026-07-21. It is an ABI bug, not a clock bug — the realtime clock only exposed it.

Measured with a temporary probe in crates/kernel/src/sync.rs cond_timedwait, splitting the timed wait from the mutex re-acquire (probe since reverted):

    [task-214] cond_timedwait tid=1 asked=2176848us wait=2176931us reacquire=0us spins=0 timed_out=true

Thirteen hits, all with the IDENTICAL asked value of 2176848 us. So:
- our wait honours the request to the microsecond
- the mutex re-acquire loop costs 0 us with 0 spins — the earlier suspicion that the unbounded re-acquire at sync.rs:406 was to blame is REFUTED by measurement
- the guest is asking us to sleep 2.18 seconds

2176848 decimal is 0x00213750 — the low 32 bits of a guest pointer, not a duration. It is stable across calls because the same stack slot is reused.

The cause: crates/libs/src/libkernel/pthread.rs registers ONE handler under BOTH names, with the Sony ABI:

    names = ["scePthreadCondTimedwait", "pthread_cond_timedwait"]
    pub fn sce_pthread_cond_timedwait(cond_ptr: u64, mutex_ptr: u64, micros: u32) -> i32

Sony's scePthreadCondTimedwait takes RELATIVE SceKernelUseconds. POSIX pthread_cond_timedwait takes a POINTER to an absolute struct timespec. The two ABIs disagree on the third argument, and we serve both with the microsecond interpretation.

Celeste imports pthread_cond_timedwait (6 occurrences in eboot.bin) and does NOT import scePthreadCondTimedwait (0 occurrences). So every call Celeste makes has its timespec pointer truncated to u32 and used as a microsecond count.

Why it only bites under the realtime clock: the bogus wait happens in both modes, but under fixed-step something signals the condvar before the bogus timeout elapses (the probe shows timed_out=true under realtime). The 2.18 s stall is therefore latent in fixed-step too and is a correctness bug independent of decision-8.

The fix is not merely to read the timespec. abstime is an ABSOLUTE deadline the guest computed from ITS clock, so it must be converted against the same virtual clock the guest read (virtual_epoch_ns in crates/libs/src/libkernel/mod.rs), not against host SystemTime. That coupling is exactly why the clock mode changes the behaviour.

Note the infrastructure already exists and the sibling API already gets this right: SyncManager::read_timespec (sync.rs:71) is used correctly by mutex_timedlock (sync.rs:309). Worth checking whether mutex_timedlock's comparison against real SystemTime has the same virtual-vs-real mismatch.

FIXED + CLOSED 2026-07-21, commit <prior-history>.

Split into two handlers on their own syscall ids (SCE_PTHREAD_COND_TIMEDWAIT 98303 keeps the Sony relative-microseconds ABI; SYS_PTHREAD_COND_TIMEDWAIT 89606 takes the POSIX abstime pointer). The POSIX handler reads the timespec and converts the absolute deadline against virtual_epoch_ns, plus EINVAL on null, EFAULT outside the arena, EINVAL on a denormalized timespec, and a zero timeout for a deadline already past. realtime then held 58.4 / 58.2 / 55.3 / 46.4 fps across the windows that previously read 52.8 / 10.1 / 0.55 / 0.46, at 96-100% emulated speed, and is now the DEFAULT clock mode.

AC WORDING vs WHAT WAS FOUND — read before trusting the ticks. AC #1 and #2 assume the frame thread was waiting on a condvar nobody signalled, and that the causal chain ran from the realtime time base to a missing wakeup. Neither is what happened. There was no missing signaller: the guest asked for a bogus 2.18 s timeout and got exactly that, so the wait expired normally (timed_out=true, re-acquire 0 us, 0 spins). The clock mode was never in the causal chain at all — it only decided whether something else happened to signal the condvar before the bogus timeout ran out, which is why fixed-step masked a bug that was always there. The ACs are ticked because the question the task exists to answer ("what blocks the frame thread") was answered conclusively by measurement, not because the mechanism matched the shape the ACs guessed.

STILL OPEN, not carried by this task: mutex_timedlock (sync.rs:309) reads its timespec correctly but compares against host SystemTime while the guest computed the deadline from the virtual clock. Same class of mismatch, different API, unaudited.
<!-- SECTION:NOTES:END -->
