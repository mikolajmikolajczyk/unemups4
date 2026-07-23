---
id: TASK-6
title: 'Threads, TLS destructors, nested guest calls under x86jit'
status: Done
assignee: []
created_date: '2026-07-09 15:05'
updated_date: '2026-07-09 20:54'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-5
priority: high
ordinal: 6000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Multi-threaded guests end-to-end. kernel/thread.rs:113-162 TLS destructors via call_guest (exec context must outlive the main run loop on that host thread); libs pthread.rs:193-233 sce_pthread_once via call_guest; worker threads RDI = entry_argument; reset exit flag before dtor calls (mirrors old vm_state.abi.should_exit = 0). Each Thread::execute host thread: fresh Vcpu over shared Arc<GuestVm>, Reg::FsBase = tls_base (no arch_prctl). Verify guest atomics/mutexes across vcpus (interpreter RMW = real host atomics).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 ps4-thread-testing.elf stdout matches baseline
- [x] #2 ps4-tls.elf stdout matches baseline
- [x] #3 RUST_LOG=debug run shows TLS destructors firing, matching native run log shape
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Branch feat/task-6-threads from main.
2. Recapture ps4-thread-testing baseline (stale: native/truncated at TEST 1). Under x86jit+park fix guest runs full suite (TESTS 1-7, Counter 40000, RWLock/CondVar/TryLock, ALL TESTS FINISHED, exit 0). Semantically verify before commit. Document oracle change in baseline header + notes + commit. Preserve other 5 baselines byte-identical.
3. Validate mechanics: TLS dtors fire (RUST_LOG=debug ps4-tls + ps4-thread-testing); sce_pthread_once via call_guest exercised (add ps4-cpu test if not); worker RDI=entry_argument + exit flag reset before dtors (verify fresh ExecCtx resets exit_requested); guest atomics/mutexes multi-vcpu (Counter 40000, 10x identical runs).
4. errno (task-11): only if a thread-path example derefs __error host pointer; else leave untouched + note.
5. Full check: run_examples.sh check all 6 clean; cargo build --release + cargo test green.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-09 (COMPLETE): threads/TLS-dtors/nested-calls validated end-to-end under x86jit. No x86jit changes needed — no lift gaps or engine bugs surfaced; pin stays at a6f6034.

BASELINE RECAPTURE (ps4-thread-testing): prior committed baseline was captured under NATIVE execution and TRUNCATED at '>>> TEST 1' (native path outran guest exit, display-thread panic race). Recaptured via run_examples.sh capture under x86jit + task-5 park fix. Only ps4-thread-testing.txt changed; the other 5 baselines verified BYTE-IDENTICAL (diffed vs pre-capture copies, no incidental regen). Oracle change documented in the baseline header (header_for ps4-thread-testing case), these notes, and the commit msg. SEMANTIC VERIFICATION of new baseline (sort -u'd SET): TESTS 1-7 all present + COMPLETE; TEST 2 'Counter Final: 40000 (Expected: 40000) -> SUCCESS' (mutex-locked increments, exact); TEST 3 Recursive SUCCESS; TEST 4 condvar ([Consumer] Data received: 12345); TEST 5 timed-wait ETIMEDOUT (110); TEST 6 detach + name verification SUCCESS + Cancel ENOTSUP + Equal OK; TEST 7 RWLock Test Passed + TryLock EBUSY (16) + free-acquire; '=== [Guest] ALL TESTS FINISHED ==='; '[SYSCALL] exit(0)'. Because the baseline is sort -u'd, a wrong counter (e.g. 39999) would emit a different line and FAIL the check — so counter==40000 is genuinely asserted.

TLS DESTRUCTORS (AC #3): thread.rs run_tls_destructors_on_exit runs each dtor as a fresh top-level run_guest_call (dtor(value), fs_base=thread TLS). Added a DEBUG log line 'Kernel: Thread N running TLS destructor 0xADDR with value 0xVAL'. RUST_LOG=debug ps4-thread-testing shows it firing 10x (one per Th_Basic worker, on that worker's OWN host thread ThreadId 04/05/06/...), each immediately followed by the guest's '[Guest TLS DTOR] called with ptr=0x1111000N' — value matches exactly what each worker set via scePthreadSetspecific. The dtor itself issues sceKernelDebugOutText (a syscall) inside the nested call, proving syscall dispatch works from within a dtor guest call on a worker context. ps4-tls does NOT use pthread-key dtors (it exercises variant-II .tdata/.tbss TLS); its dtor-firing evidence comes from thread-testing.

sce_pthread_once (call_guest): exercised by thread-testing — '[Guest ONCE] once_init running (should appear exactly once)' appears EXACTLY ONCE across 10 concurrent scePthreadOnce callers (correct once-semantics + nested call_guest). Additionally added a focused ps4-cpu integration test ac_e_pthread_once_nested_from_workers: 8 WORKER host threads each run_guest_call into a SYS_ONCE stub whose handler does a guarded nested call_guest(INNER); asserts every worker returns 99 and the init ran exactly once — proving nested call_guest off the main host thread with correct per-thread thread-local exec context (no bleed).

WORKER RDI + EXIT-FLAG RESET: worker RDI = entry_argument (thread.rs:70-74, is_main ? start_rsp : entry_argument) — confirmed. Exit-flag reset: run_guest_call installs a FRESH ExecCtx (exit_requested: None) per call and restores the prior on return (exec.rs:104-120), so a worker's sce_pthread_exit request cannot leak into the subsequent dtor run_guest_calls — mirrors old should_exit=0 by construction, no code change needed. Added ps4-cpu test ac_f_exit_flag_reset_between_calls: first call requests thread-exit (ThreadExit(7)), a second call on the SAME host thread returns normally (Returned(42)) — proves no stale-flag leak.

GUEST ATOMICS/MUTEXES ACROSS VCPUS: host-thread-per-guest-thread, fresh Vcpu over shared Arc<GuestVm> (interpreter RMW = real host atomics). TEST 2 spawns 4 MutexWorker threads doing lock'd increments to exactly 40000. STABILITY: ran ps4-thread-testing 10x consecutively; the normalized guest test-event SET hashed IDENTICAL all 10 runs (md5 eead6f62...), counter 40000 every time. run_examples.sh check: all 6 examples OK (hello_world, ps4-fs, ps4-mmap, ps4-tls, ps4-thread-testing, ps4-softgpu).

ERRNO / task-11: did NOT bite on the thread path. Neither ps4-thread-testing nor ps4-tls dereferences the __error/errno host pointer (no UnmappedMemory, no Fatal). ETIMEDOUT(110)/EBUSY(16) in the guest come straight from our syscall return values, not from reading *__error. task-11 left To Do (still latent; note added there).

WORKSPACE: cargo build --release green; cargo test green (ps4-cpu 6/6 incl. 2 new task-6 tests, ps4-loader 3/3, ps4-memory 7/7). rustfmt clean on changed files; ps4-cpu/ps4-kernel clippy: no new warnings from these changes (pre-existing style nits + ps4-gpu ash unsafe warnings untouched).
<!-- SECTION:NOTES:END -->
