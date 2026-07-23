---
id: TASK-10
title: Guest-fault diagnostics polish
status: Done
assignee: []
created_date: '2026-07-09 15:06'
updated_date: '2026-07-09 22:18'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-9
priority: low
ordinal: 10000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Turn remaining Exit variants into actionable reports in crates/cpu/src/exec.rs (VMA-name lookup possibly via kernel bridge). UnmappedMemory{addr} -> faulting RIP, access kind, nearest VMA names from memory manager; Exception{vector} -> signal-style names; UnknownInstruction -> disassembly of surrounding bytes + hint to open an x86jit issue with the bytes; optional budget watchdog behind env var (prints RIP periodically for hang debugging).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 null-deref test produces report containing RIP, address, and VMA context
- [x] #2 ud2 produces an Exception report with vector name
- [x] #3 examples unaffected (baselines still clean)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. exec.rs: turn each fatal Exit variant into a multi-line actionable report via tracing::error! + same string in GuestExit::Fatal.
2. VMA lookup route: chose option (a) — a set_fault_annotator(Box<dyn Fn(u64)->String>) OnceLock installed from app main, backed by VmMemoryManager. Keeps ps4-cpu free of a ps4-memory/ps4-core dep (mirrors set_syscall_dispatch); cleaner than routing through the kernel bridge.
3. VMA logic lives in ps4-core trait method describe_fault_context (default fallback) overridden by VmMemoryManager (containing / preceding / following VMA + below-guest_base / above-span host-pointer-leak hint).
4. Exception vector -> (mnemonic, description, signal) mapping in exec.rs.
5. ud2/int3/int1: real x86jit engine gap — they are architectural exceptions, not lift gaps. Added IrOp::Trap{vector} in x86jit (interp + cranelift + MemCtx.exception_vector out-field); pin bumped.
6. Watchdog behind UNEMUPS4_WATCHDOG=<blocks>: run with Some(budget); BudgetExhausted logs RIP rate-limited and resumes; default None = zero overhead.
7. Tests in ps4-cpu (null-deref, ud2) + x86jit tests (ud2/int3/int1 under both backends).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented on branch feat/task-10-diagnostics.

Diagnostics design — VMA lookup route: option (a), a fault-address annotator
(set_fault_annotator, OnceLock<Box<dyn Fn(u64)->String + Send + Sync>>) installed from
app main, backed by VmMemoryManager. ps4-cpu keeps depending only on x86jit (no cycle),
mirroring set_syscall_dispatch. The rich VMA logic lives in ps4-core's new trait method
VirtualMemoryManager::describe_fault_context (default = bare fallback), overridden by
VmMemoryManager: names the containing VMA, or the nearest preceding/following region in
a gap, plus a "below guest_base" (null deref) / "at/above span — host pointer leaked?"
(task-11 class) hint when addr is outside [guest_base, span).

Reports (all via tracing::error!, same content in GuestExit::Fatal):
- UnmappedMemory: "guest fault: UnmappedMemory (read) of 0x0 / faulting instruction:
  rip 0x400000 (mov 0x0,%rax) / VMA context: address 0x0 is below guest_base (0x10000)…"
- Exception: "guest fault: Exception vector 6 (#UD — invalid opcode, SIGILL) at 0x400000
  / faulting instruction: rip 0x400000 (ud2)". Vector→(mnemonic,desc,signal) map covers
  #DE/#DB/#BP/#OF/#BR/#UD/#NM/#DF/#GP/#PF/#MF/#AC/#XM.
- UnknownInstruction: disasm of the faulting bytes + "ACTION: file a task in the x86jit
  backlog to lift this opcode, with the bytes: <hex>" (ready-to-paste byte string).

Watchdog: UNEMUPS4_WATCHDOG=<blocks>. Unset → budget None → cpu.run unbounded (zero
overhead, unchanged hot path). Set → run with Some(N); Exit::BudgetExhausted is a
cooperative resume that logs RIP every 64th tick (rate-limited) and `continue`s — it
never falls through to format_fatal and never disturbs the Syscall/Hlt resolution.

x86jit engine change (real gap): ud2/int3/int1 were surfacing as UnknownInstruction, but
they are architectural exceptions. Added IrOp::Trap{vector} lifted from Ud2→6/#UD,
Int3→3/#BP, Int1→1/#DB; interp returns Exit::Exception directly; JIT carries the vector
through a new append-only MemCtx.exception_vector out-field (offset 80) that the div (#DE)
path now also sets (0). Both backends agree (x86jit-tests/tests/jit.rs). Committed in the
x86jit-migration worktree as e58f23e; unemups4 pin bumped a6f6034→e58f23e.

Verification: cargo test --workspace green (ps4-cpu run_guest incl. ac_g_null_deref_report
+ ac_h_ud2_exception_report; ps4-memory). x86jit core+cranelift+jit suites green.
scripts/run_examples.sh check = 6/6 match baselines. scripts/diff_backends.sh = 6/6
interp==jit.

Follow-up (pin bump e58f23e→6cccf64, branch chore/bump-x86jit-6cccf64): x86jit task-194
made the saved RIP HW-accurate for the trap/fault distinction — a FAULT (#UD/ud2, #DE)
leaves Exit::Exception.addr (== vcpu RIP) ON the faulting instruction; a TRAP (#BP/int3,
#DB/int1) now resumes PAST it. exec.rs's report_exception disassembled at that RIP for
the "faulting instruction" line, which for a trap would name the WRONG (next) instruction.
Reconciled: report_exception now branches on is_trap_vector (1|3); for a trap it labels
the RIP the "resume address AFTER the instruction" and back-disassembles at rip-1 (the
1-byte int3/int1) so the report still names the real trapping instruction. Faults are
unchanged. New test ac_i_int3_trap_report (int3 → #BP, reports CODE+1 resume RIP + the
trap annotation + back-disassembled int3 at CODE); ac_h_ud2_exception_report gained a
negative assertion that the fault path carries NO after-instruction annotation. Full
verify green again: cargo test --workspace, run_examples 6/6, diff_backends 6/6.
<!-- SECTION:NOTES:END -->
