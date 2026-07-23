---
id: TASK-11
title: Guest-resident errno slot (sys_errno returns host pointer)
status: Done
assignee: []
created_date: '2026-07-09 18:19'
updated_date: '2026-07-10 11:24'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-5
priority: high
ordinal: 11000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
libkernel/mod.rs:60 sys_errno/__error returns addr_of_mut!(ERRNO_VAL) as u64 — a host static pointer handed to guest code. Under x86jit the guest deref of that pointer is outside [GUEST_BASE, span) and traps UnmappedMemory. Not yet reachable (guest currently dies at CRT FWAIT, x86jit TASK-194), but will bite as soon as the CRT runs. Fix: allocate a guest-resident errno slot (per-thread — likely in the TLS block or a small guest page keyed by tid) and return its guest address; handlers writing errno must write through guest memory. Found during task-5 host-pointer audit.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 sys_errno returns a guest address inside [GUEST_BASE, span)
- [x] #2 errno is per-thread correct (two threads see independent errno)
- [x] #3 examples that exercise errno (ps4-fs error paths) pass baselines
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. cpu/exec.rs: add errno_addr to ExecCtx; add param to run_guest_call; call_guest inherits; add pub current_errno_addr() accessor; export from lib.rs. 2. kernel/thread.rs: grow TLS alloc by 16B, place errno slot after TCB (fs_base+tcb_size), zero it, add Thread.errno_base, pass to all run_guest_call sites. 3. libs/libkernel/mod.rs: delete ERRNO_VAL, sys_errno returns ps4_cpu::current_errno_addr().unwrap_or(0). Verify build/clippy/test/fmt + run_examples baselines (ps4-fs).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
task-6 (2026-07-09): still latent — did NOT bite on the thread path. ps4-thread-testing and ps4-tls both run to a clean exit(0) with NO deref of the __error/errno host pointer (no UnmappedMemory, no Fatal). ETIMEDOUT(110)/EBUSY(16) values the guest prints come straight from our syscall return values, not from reading *__error. Left To Do; will likely bite on a CRT error path (e.g. ps4-fs error branches, or GPU/game code in task-7+). No fix applied.

2026-07-10: IMPLEMENTED. Per-thread guest errno slot lives in each thread's TLS allocation just past the TCB (`errno_base = fs_base + tcb_size`, allocation grown 16B, zero-initialized in kernel/thread.rs). exec.rs carries `errno_addr` in `ExecCtx` (installed by `run_guest_call`, inherited by nested `call_guest`); new `pub current_errno_addr()` reads it. libkernel `sys_errno`/`__error` now returns `ps4_cpu::current_errno_addr().unwrap_or(0)` — a guest-resident address, not the deleted host `ERRNO_VAL` static. Handlers unchanged (they return negative errno as their value; nothing writes the slot, so the CRT reads 0 — matches baselines).

Verify: `cargo build --release` green; `cargo test -p ps4-cpu` 9/9 (run_guest with the new param); task-11 crates (ps4-cpu/kernel/libs) clippy `-D warnings` clean; task-11 files fmt-clean. Oracle `run_examples.sh check`: hello_world + ps4-mmap OK; ps4-fs guest output (FD read, 'Hello, PS4 World from HLE!', Data Integrity PASSED) bit-identical to baseline; ps4-tls / ps4-thread-testing guest logic bit-identical. The 4 "diverged" reports are ONE environment line each — `ps4_gpu::display: Failed to initialize Vulkan: Unable to find a Vulkan driver` (session had no Vulkan ICD; baselines captured with a driver) — unrelated to errno. AC#1/#2/#3 met.

Pre-existing debt observed (NOT task-11, left untouched): clippy `-D warnings` fails in ps4-core (memory.rs:69 missing `# Safety`, pad.rs:40 `new` without `Default`); rustfmt drift in ~8 files (core/gpu/loader/syscalls, memory test); ps4-gpu emits 59 edition-2024 `unsafe_op_in_unsafe_fn` warnings.
<!-- SECTION:NOTES:END -->
