---
id: TASK-216
title: >-
  hle/kernel: audit every timed wait — scePthreadMutexTimedlock has the inverted
  ABI, mutex_timedlock uses host time, sem_timedwait ignores its deadline
status: Done
assignee: []
created_date: '2026-07-21 22:04'
updated_date: '2026-07-22 14:23'
labels:
  - hle
  - kernel
  - correctness
dependencies: []
priority: medium
ordinal: 221000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-214 found that pthread_cond_timedwait shared a handler with Sony's scePthreadCondTimedwait despite an incompatible third argument, and slept a truncated guest pointer as a 2.18 s duration. That was not a one-off. Three more defects of the same class are confirmed in the timed-wait surface, and they were found by looking rather than by anything failing.

1. scePthreadMutexTimedlock has the ABI INVERTED — the mirror of the task-214 bug. The Sony header (data/oo_sdk/include/orbis/libkernel.h:629) declares:

       int32_t scePthreadMutexTimedlock(OrbisPthreadMutex*, OrbisKernelUseconds);

   Relative microseconds. Our handler (crates/libs/src/libkernel/pthread.rs:482) takes abstime_ptr and passes it to a kernel routine that dereferences it as a timespec. A guest calling this passes a small integer where we expect a pointer, so the read lands on an unmapped low address and yields EFAULT, or worse resolves to garbage. Note the same header confirms scePthreadCondTimedwait as OrbisKernelUseconds, which is what the task-214 split was built on, so the header is a trustworthy oracle here.

2. mutex_timedlock compares against HOST time (crates/kernel/src/sync.rs:343): it reads the guest timespec correctly, then does SystemTime::now() and target_time.duration_since(now). The guest computed that absolute deadline from clock_gettime, which we back with virtual_epoch_ns — the VIRTUAL clock, whose rate depends on UNEMUPS4_CLOCK (decision-8). Comparing a virtual-epoch deadline against host wall time is the same virtual-vs-real mismatch task-214 had to avoid, and its severity now varies with the clock mode.

3. sem_timedwait ignores its timeout entirely (crates/libs/src/libkernel/sema.rs:123): the parameter is named _abstime and the body blocks exactly like sem_wait. A timed wait that cannot time out is a deadlock waiting for a guest that relies on the timeout to make progress. The existing comment argues this is fine for a permit that eventually arrives, which is precisely the case where the timeout does not matter — it says nothing about the case the API exists for.

Priority is medium, not high, because there is no evidence Celeste depends on any of the three today: neither scePthreadMutexTimedlock nor sem_timedwait appears in the profiler's per-syscall table across several gameplay runs. Treat that as weak evidence only — symbols are imported by NID, so grepping the binary for a name proves nothing (sceKernelWaitSema is called 7685 times in a single run while its name string does not appear in eboot.bin at all). The profiler is the only reliable oracle for what is actually called.

Work:
- fix each of the three, verifying every timed-wait signature against the Sony/OpenOrbis headers rather than against what the current code assumes
- convert any ABSOLUTE deadline against virtual_epoch_ns, never host SystemTime, exactly as the task-214 POSIX handler now does
- give POSIX and Sony spellings SEPARATE handlers wherever their ABIs differ; the generated syscall table already carries distinct ids for both (SYS_* and SCE_*)
- sweep the rest of the timed surface for the same two failure shapes — one handler serving two ABIs, and an absolute deadline compared against the wrong clock

The deeper fix worth considering: the ps4_syscall macro accepts a names list, which is what let one handler silently serve two incompatible ABIs. If that list can be restricted, or a signature mismatch made loud, this class stops recurring instead of being audited again.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 scePthreadMutexTimedlock takes relative microseconds per the SDK header, not an abstime pointer
- [x] #2 every absolute deadline in a timed wait is converted against virtual_epoch_ns rather than host SystemTime
- [x] #3 sem_timedwait honours its deadline and returns ETIMEDOUT, or the notes state why it cannot
- [x] #4 the remaining timed-wait surface is swept for both failure shapes and the result recorded, including any API left deliberately unfixed
- [x] #5 build + clippy clean, cargo test --workspace green; a test pins at least one absolute-deadline conversion against the virtual clock
<!-- AC:END -->



## Notes

### 2026-07-22 — all three fixed, plus one more found in the sweep

**#1 `scePthreadMutexTimedlock` ABI.** Confirmed against `data/oo_sdk/include/orbis/libkernel.h:629`
(`OrbisPthreadMutex*, OrbisKernelUseconds`, and `OrbisKernelUseconds` is `uint32_t` per
`_types/kernel.h:154`). The handler now takes relative micros. A separate POSIX
`pthread_mutex_timedlock` was added on `SYS_PTHREAD_MUTEX_TIMEDLOCK` — that spelling was not
bound at all before, so a guest importing it got a missing symbol.

**#2 host clock in `mutex_timedlock`.** Fixed by moving the SEAM rather than the comparison. The
kernel entry point now takes relative microseconds, exactly like `cond_timedwait` already did, so
the absolute-deadline conversion happens once in the libs layer against `virtual_epoch_ns` and the
kernel has no notion of an absolute deadline to compare against the wrong clock. `SystemTime` and
`read_timespec` are gone from `crates/kernel/src/sync.rs`. The relative deadline is fixed at entry
and not recomputed per wakeup, so a spurious wakeup or a lock handed to another thread cannot
extend the total wait.

**#3 `sem_timedwait`.** `HostSem` gained `wait_timeout`; the handler converts the absolute deadline
and returns -1 with `errno = ETIMEDOUT` (60, FreeBSD — Linux's 110 makes Mono abort, per the
sibling `sem_trywait` bug). Same fixed-deadline discipline.

**FOUND IN THE SWEEP AND FIXED — `sceKernelWaitSema`.** Same defect shape as #3: the signature is
`(sema, need, OrbisKernelUseconds *timeout)` (`libkernel.h:449`) and the timeout pointer was
ignored outright. Now honoured; NULL still means wait forever, and a non-null pointer outside the
guest arena falls back to the infinite wait rather than dereferencing it or failing the call. This
one matters more than the other three combined for the current title: Celeste calls it **26,882
times per run** with an average blocked time of 38.9 ms, while `scePthreadMutexTimedlock` and
`sem_timedwait` never appear in the profiler at all.

**The abstime conversion now exists once.** `abstime_to_relative_micros` (null → EINVAL, outside
arena → EFAULT, denormalized → EINVAL, past deadline → 0) is shared by `pthread_cond_timedwait`,
`pthread_mutex_timedlock` and `sem_timedwait`. It had one copy before this task and would have
grown two more.

### AC#4 — the rest of the timed surface, and what was left alone

| API | State | Why |
|---|---|---|
| `sceKernelWaitEqueue` | timeout ignored, LEFT | It does not wait on a real queue at all. The handler synthesizes completions and falls back to a paced `sleep(16ms)` (doc-6 Phase A). There is nothing yet for a timeout to time out against; honouring it in isolation would be theatre. Fix it when the equeue model becomes real. |
| `scePthreadRwlockTimedrdlock` / `Timedwrlock` | not bound | Declared `OrbisKernelUseconds` in the header, but we bind only the untimed `Rdlock`/`Wrlock`. A missing binding fails loudly through the missing-symbol path, unlike a wrong one, so this is a gap rather than a defect. |
| `scePthreadSemTimedwait` (`SCE_PTHREAD_SEM_TIMEDWAIT`) | not bound | Same: absent, not wrong. |
| `pthread_cond_reltimedwait_np`, `pthread_mutex_reltimedlock_np`, `sem_reltimedwait_np` | not bound | The RELATIVE variants exist in the generated name table. Worth binding when something calls them — they need no conversion at all, the seam is already relative. |
| `sceKernelUsleep` / `nanosleep` | not audited here | Sleeps, not waits: no deadline compared against a clock, so neither failure shape applies. |

**The deeper fix is NOT done.** The task suggested restricting the `ps4_syscall` `names` list or
making a signature mismatch loud, so one handler cannot silently serve two incompatible ABIs. That
is what produced both task-214 and defect #1 here, and this task only removed the instances it
could see. Left as a separate change: it is a macro-level design decision, not a bug fix, and it
belongs with a survey of which shared-name handlers are legitimately identical.

### Verification

`cargo test --workspace` 580 passed (3 new), clippy clean on the touched crates. Smoke run to
gameplay: no fatal, no panic, no missing symbol; 41-48 fps with `guest_exec` 15.6-19.0 ms, i.e. no
regression on the `sceKernelWaitSema` path this change touches hardest.

CAVEAT on AC#5. The test pins the arithmetic and the REFERENCE — the deadline is built from
`virtual_epoch_ns` exactly as a guest builds it from `clock_gettime` — but it does not compare the
two clock modes side by side. `UNEMUPS4_CLOCK` is process-global state owned by `ps4-core` and
resolved once, so `ps4-libs` cannot force fixed-step from a unit test without a test hook there. A
host-time implementation would still pass this test under the default realtime mode and fail only
under fixed-step. Closing that would mean exporting a mode-reset helper from `ps4-core`, which is
worth doing when something else needs it.
