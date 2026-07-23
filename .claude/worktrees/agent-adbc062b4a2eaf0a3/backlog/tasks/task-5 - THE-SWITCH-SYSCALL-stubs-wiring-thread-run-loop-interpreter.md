---
id: TASK-5
title: 'THE SWITCH: SYSCALL stubs + wiring + thread run loop (interpreter)'
status: Done
assignee: []
created_date: '2026-07-09 15:05'
updated_date: '2026-07-09 20:37'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-1
  - TASK-4
priority: high
ordinal: 5000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Flip execution to x86jit interpreter. Stub emitters (kernel/src/hle.rs write_stub + loader/src/linker.rs:152-207 lazy stubs) -> B8 <id> 0F 05 C3 (MOV EAX,id; SYSCALL; RET, NOP-pad to 32B); drop trampoline_addr/set_trampoline plumbing. app/main.rs: Arc<GuestVm> + VmMemoryManager instead of LinuxMemoryManager; delete trampoline wiring (lines 72-74); set_syscall_dispatch(ps4_libs::rust_syscall_handler); new Process.guest_vm field. kernel/thread.rs execute() -> run_guest_call, main thread rdi = start_rsp; pthread.rs sce_pthread_exit -> request_thread_exit. Keep run_tls_destructors_on_exit and sce_pthread_once compiling via call_guest (fully validated in task-6). Missing-symbol marker 0xC000_0000 logic unchanged. Note: stray real syscall instructions in guest code now trap into the dispatcher instead of the host kernel. Risk: Exit::UnknownInstruction on Orbis CRT instructions -> small x86jit lift additions, budget 1-2 iterations. Also grep handlers for host-allocated pointers handed to guest (known: main.rs:83 &ARGC_ZERO, ignored for main).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 hello_world.elf stdout diffs clean vs task-1 baseline
- [x] #2 ps4-fs.elf and ps4-mmap.elf stdout diff clean vs baselines
- [x] #3 deliberately-unresolved import still produces [FATAL ERROR] ... missing symbol (0xC000_0000 marker intact)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Branch feat/task-5-switch from main.
2. Stub emitters -> SYSCALL: hle.rs write_stub + linker.rs lazy-stub gen emit B8 <id LE> 0F 05 C3, NOP-pad to 32B; drop trampoline_addr/set_trampoline plumbing. Stub writes route via memory.write_bytes (already SMC-tracked via VmMemoryManager).
3. main.rs: build Arc<GuestVm>(DEFAULT_SPAN)+VmMemoryManager instead of LinuxMemoryManager; drop trampoline wiring; set_syscall_dispatch(thin fn wrapping rust_syscall_handler); store Arc<GuestVm> on Process; drop &ARGC_ZERO entry_arg for main.
4. process.rs: new guest_vm field + accessor.
5. thread.rs execute(): run_guest_call(vm, entry, start_rsp, rdi=start_rsp for main / entry_argument for worker, fs_base=tls_base); map GuestExit->exit value; drop arch_prctl/VmState from new path (old fns stay, unreachable).
6. pthread.rs: sce_pthread_exit -> request_thread_exit; sce_pthread_once -> call_guest (nested); TLS dtors (thread.rs) -> call_guest.
7. Keep old asm compiling (unreachable).
8. Iterate examples via run_examples.sh check; host-pointer audit; verify missing-symbol path; x86jit gaps -> backlog tasks in x86jit repo.
9. cargo build --release + cargo test green.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-09 (COMPLETE): x86jit switch validated end-to-end. Iteration log:

X86JIT COMMITS (worktree feat/unemups4-migration): a6f6034 feat(core): lift FWAIT/WAIT (0x9B) as x87 sync no-op (task-194). Lifts iced Mnemonic::Wait (0x9B) to zero IR ops alongside Nop/Pause/Endbr; cranelift needs no codegen. Tests: interp + jit no-op + differential-vs-Unicorn; ISA compat coverage.json regenerated. x86jit full suite green (379 passed, 2 skipped, fuzz_robustness excluded per repo rule); clippy + fmt clean.

REV BUMP: unemups4 Cargo.toml x86jit-core/x86jit-cranelift pin 1c4a1c5 -> a6f6034; cargo update -p x86jit-core; workspace builds --release + tests green (ps4-cpu 4/4, ps4-loader 3/3, ps4-memory 7/7).

GAP LIST (only one x86jit lift gap surfaced): FWAIT/WAIT 0x9B, Orbis CRT __libc_start_main padding after __init_libc, all 6 ELFs -> x86jit task-194, fixed as a no-op lift. No further UnknownInstruction/UnmappedMemory/Fatal gaps appeared for the three targets.

ERRNO / task-11: did NOT block. hello_world/ps4-fs/ps4-mmap run to clean exit(0) without ever dereferencing the __error/errno host pointer (libkernel/mod.rs:60). The prior prediction it would trap did not materialize for these examples. task-11 left To Do (latent; likely bites task-6/7).

AC #3 (missing-symbol): verified at runtime via a temporary env-gated force-miss hook (UNEMU_FORCE_MISS=<sym>, NOT committed) forcing sceKernelDebugOutText unresolved -> linker stub (ID 0xc0000000) -> guest SYSCALL -> rust_syscall_handler -> '[FATAL ERROR] ... called a missing symbol: sceKernelDebugOutText'. Hook reverted.

UNEMUPS4-SIDE FIX (app/unemups4/src/main.rs): wrapped run_display_loop in catch_unwind; on a headless-devShell winit/Wayland panic the main thread now PARKS instead of aborting, so the guest (on the emulator thread) reaches its own exit() syscall and its output is captured. Without this the display-thread panic raced the guest and truncated output (the interpreter, being slower than the old native path, lost this race consistently). This made run_examples.sh check deterministic.

FINAL run_examples.sh check (3x, stable): OK hello_world, OK ps4-fs, OK ps4-mmap, OK ps4-tls, OK ps4-softgpu; FAIL ps4-thread-testing ONLY. The thread-testing FAIL is NOT a regression: its committed baseline was captured NATIVE and truncated at 'TEST 1' by the old display race; under x86jit + the park fix the guest now runs the FULL suite (TESTS 1-7 COMPLETE, Counter 40000 SUCCESS, RWLock/CondVar/TryLock all pass, 'ALL TESTS FINISHED', exit(0)) — strictly MORE output than the stale baseline. Not a target of task-5; baseline not recaptured (out of scope; recapture is a deliberate oracle change). Left for task-6.

unemups4 now pins x86jit rev a6f6034af4781e3b97aeda46b897c6f6a3342206.
<!-- SECTION:NOTES:END -->
