---
id: TASK-3
title: Rework crates/cpu into safe x86jit wrapper
status: Done
assignee: []
created_date: '2026-07-09 15:05'
updated_date: '2026-07-09 16:44'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-2
priority: high
ordinal: 3000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
New execution core in ps4-cpu (old asm kept, unused): guest_vm.rs — GuestVm::new(span): identity mmap (task-2), Vm::with_backend_host_ram(VmConfig::reserved+guest_base, interpreter), pre-map whole arena RWX/Ram, HLT gadget page at guest 0x30000, GuestCpuFeatures::v2(), then Arc. exec.rs — run loop: Exit::Syscall -> fill NativeContext from 15 GPRs -> dispatch -> set_reg(Rax) -> check thread-local exit flag; Hlt at gadget+1 -> Returned(rax); other exits -> Fatal with rip/addr/hexdump context. context.rs — NativeContext moved from crates/libs, re-exported from ps4-libs. API: set_syscall_dispatch(fn(u64, &mut NativeContext) -> u64) via OnceLock (avoids cpu->libs dep); run_guest_call(vm, entry, rsp, rdi, fs_base) -> GuestExit{Returned|ThreadExit|Fatal}; call_guest(entry, arg) nested via fresh Vcpu at cur_rsp-128 from thread-local exec context; request_thread_exit(value) replaces should_exit/VmStateAbi.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 cargo test -p ps4-cpu: hand-assembled guest mov eax,42; ret via run_guest_call returns 42
- [x] #2 SYSCALL stub dispatches to test handler; all six args readable incl. RCX (arg3, not clobbered); return lands in guest RAX
- [x] #3 nested call_guest from inside a handler returns inner value
- [x] #4 request_thread_exit(7) yields GuestExit::ThreadExit(7)
- [x] #5 full workspace still builds; native path untouched
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
New ps4-cpu modules on branch feat/task-3-cpu-wrapper (old asm untouched): (1) hostmem.rs - replicate reserve_at mmap helper inline (avoid dragging heavy x86jit-linux shim/thread/proc). (2) guest_vm.rs - GuestVm::new(span=64GiB, guest_base 0x10000): identity mmap, Vm::with_backend_host_ram(reserved+interpreter, v2 features, Fast), pre-map [0x10000,span) RWX/Ram before Arc, HLT gadget (F4) at 0x30000; expose write/read_bytes, consts, new_vcpu. (3) context.rs - NativeContext moved verbatim from libs; libs re-exports. (4) exec.rs - set_syscall_dispatch(OnceLock), run_guest_call->GuestExit{Returned|ThreadExit|Fatal}, call_guest nested, request_thread_exit; run loop fills NativeContext from 15 GPRs, dispatches, Hlt@gadget+1->Returned, else Fatal w/ disasm. (5) lib.rs exports new API alongside old. Tests: 4 AC hand-assembled guests. Workspace build/test green.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE on branch feat/task-3-cpu-wrapper (commit <prior-history>, GPG-signed, pre-commit passed; NOT merged/pushed). Old native path (launch.s/trampoline.s/vmstate.rs) left compiled and functional — still the active path until task-5.

API as implemented in ps4-cpu (all re-exported from crate root):
- GuestVm::new(span) -> Arc<GuestVm>: identity mmap [GUEST_BASE=0x10000, span) via a self-contained reserve_at (MAP_FIXED_NOREPLACE|NORESERVE, asserts exact addr), Vm::with_backend_host_ram(VmConfig::reserved(span) + InterpreterBackend), whole-arena RWX/Ram pre-map BEFORE Arc, HLT gadget byte 0xF4 at guest 0x30000, GuestCpuFeatures::v2(), MemConsistency::Fast (set on VmConfig). Exposes write_bytes/read_bytes (via vm, SMC-tracked), new_vcpu, vm(), span()/guest_base()/gadget_addr(), plus pub consts GUEST_BASE/DEFAULT_SPAN(64 GiB)/GADGET_ADDR/GADGET_BYTE.
- set_syscall_dispatch(fn(u64,&mut NativeContext)->u64): global OnceLock; double-set logged+ignored (never panics).
- run_guest_call(vm,entry,rsp,rdi,fs_base)->GuestExit{Returned(rax)|ThreadExit(val)|Fatal(String)}: fresh vcpu, gadget addr pushed at rsp-8 as return addr, Reg::{Rip,Rsp,Rdi,FsBase} set; loop over cpu.run(vm,None); Syscall->fill NativeContext from 15 GPRs (order matches trampoline.s / NativeContext repr(C))->dispatch->set Rax->refresh cur_rsp->check exit flag; Hlt at gadget+1->Returned(rax); else->Fatal with RIP + UnknownInstruction hexdump/disasm (x86jit_core::disassemble).
- call_guest(entry,arg)->u64: nested call from a handler; reads thread-local exec ctx, fresh vcpu at (cur_rsp-128)&!0xF, same gadget/run loop (nested syscalls dispatch too). Panics if no active exec context (documented).
- request_thread_exit(value): sets a thread-local flag the run loop consumes -> ThreadExit. Replaces should_exit/VmStateAbi.
- NativeContext moved verbatim from ps4-libs (same repr(C) layout, arg0..arg5 incl. RCX=arg3, same FromReg impls). ps4-libs re-exports under crate::context so all handlers compile unchanged; old libs/src/context.rs deleted.

DEVIATIONS from spec: none functional. Minor: exec context is stored per-thread (as specified); DEFAULT_SPAN const provided but not yet consumed (app main wires it in task-5).

x86jit-linux DEP DECISION: did NOT depend on x86jit-linux. Its lib.rs pulls in shim.rs (~146 KiB syscall shim), thread.rs (~35 KiB), proc.rs, sigsegv.rs — the full Linux userland embedder — none of which ps4-cpu wants (unemups4 has its own HLE/threading/loader). reserve_at is a ~30-line mmap wrapper, so it was replicated verbatim in ps4-cpu::hostmem, keeping deps to just x86jit-core + libc + tracing.

architecture.md: NOT edited (per 'only if now wrong'). Line 30 describes the CURRENT runtime — native direct-on-host execution + FS-swap trampoline — which is still the active path until task-5. The new x86jit path is dormant/unused, so the doc stays accurate. Will be updated in task-5/task-8.

TESTS (cargo test -p ps4-cpu, all pass): ac_a_returns_immediate (mov eax,42;ret -> Returned(42)); ac_b_syscall_args_and_return (SYSCALL stub, all six args RDI/RSI/RDX/RCX/R8/R9 captured incl. RCX survives, return DEADBEEF lands in RAX); ac_c_nested_call_guest (handler call_guest(INNER)->99); ac_d_thread_exit (request_thread_exit(7)->ThreadExit(7)). Note: tests serialize VM construction via a Mutex since the fixed identity mmap is a process-global singleton.

WORKSPACE: cargo build --release green (native path untouched); cargo test workspace green (all crates). ps4-cpu clippy clean on new files (remaining warnings are pre-existing in vmstate.rs, the old path). Pre-existing ps4-gpu warnings unrelated.

DEFERRED to task-5: wiring GuestVm/set_syscall_dispatch into app main, SYSCALL stub emission in loader/kernel, thread.rs execute() -> run_guest_call, pthread_exit -> request_thread_exit.
<!-- SECTION:NOTES:END -->
