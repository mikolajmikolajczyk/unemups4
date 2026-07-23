---
id: TASK-113.2
title: >-
  diag: why-did-it-stop reporter (missing symbol / unmapped addr+VMA / unknown
  instr)
status: In Progress
assignee: []
created_date: '2026-07-14 08:27'
updated_date: '2026-07-14 19:17'
labels:
  - retail
  - diagnostics
dependencies: []
parent_task_id: TASK-113
priority: high
ordinal: 114000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Cross-cutting, lands early (parallel with FASE 0) because it sets the pace of every later triage loop. When the guest stops, emit a legible report instead of a bare error: missing import -> module + NID + resolved-or-not; unmapped access -> RIP + faulting addr + nearest VMA name/bounds; UnknownInstruction -> RIP + bytes + short disasm of the surrounding window + 'file against x86jit' hint; unhandled syscall -> number + args. Drives FASE 2+ (unknown-depth runtime bring-up) — the faster each wall is legible, the faster we file+patch. Reuse existing fault plumbing; no new execution behavior.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 a missing import reports module + symbol/NID and whether it resolved
- [ ] #2 an unmapped access reports RIP, faulting address, and the nearest VMA (name + bounds)
- [ ] #3 an UnknownInstruction reports RIP + raw bytes + a short disasm window + an x86jit-report hint
- [ ] #4 an unhandled syscall reports its number and arguments
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Guest backtrace added to fatal reports (crates/cpu/src/exec.rs guest_backtrace): walks the RBP frame chain from the faulting frame, reads *(rbp)=caller rbp and *(rbp+8)=return addr, attributes each via the VMA annotator (module name + offset), up to 12 frames. Appended to ALL format_fatal outputs (UnmappedMemory/Exception/UnknownInstruction/etc). Immediately proved its worth: identified a libc int-0x44 abort as __cxa_guard_release via the caller chain (frame #0 = the aborting libc function). Prior state (from task-113): NID->name in missing-symbol log + FATAL (commit 3a86a58); unmapped-addr already carries VMA context. Still TODO for full AC: unhandled-syscall reporting nicety (missing-symbol already fatal-reports the NID+name).
<!-- SECTION:NOTES:END -->
