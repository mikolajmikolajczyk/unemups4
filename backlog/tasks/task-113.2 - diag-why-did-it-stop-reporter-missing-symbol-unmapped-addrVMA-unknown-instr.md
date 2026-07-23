---
id: TASK-113.2
title: >-
  diag: why-did-it-stop reporter (missing symbol / unmapped addr+VMA / unknown
  instr)
status: Done
assignee: []
created_date: '2026-07-14 08:27'
updated_date: '2026-07-23 18:45'
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
- [x] #1 a missing import reports module + symbol/NID and whether it resolved
- [x] #2 an unmapped access reports RIP, faulting address, and the nearest VMA (name + bounds)
- [x] #3 an UnknownInstruction reports RIP + raw bytes + a short disasm window + an x86jit-report hint
- [x] #4 an unhandled syscall reports its number and arguments
- [x] #5 the faulting RIP is attributed to module + nearest exported symbol + offset (module!symbol +0xNN), not just a VMA offset
- [x] #6 every rbp-chain backtrace frame gets the same module!symbol +offset attribution
- [x] #7 a symbol name that cannot be recovered from its NID reports the NID and offset instead, never an invented name
- [x] #8 a fatal fault dumps the faulting thread's last 32 HLE calls with id/name, register arguments, and return value
- [x] #9 an HLE call that never returned (handler faulted or wedged) is shown as in-flight rather than omitted
- [x] #10 the breadcrumb ring is always on, per-thread, fixed-size, and allocates nothing on the dispatch path
<!-- AC:END -->

















## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
PRIORITY BUMPED 2026-07-16: the single-line why-did-it-stop reporter is the highest-leverage log-context fix — one structured line (STOPPED reason=... rip=... last-submit=... missing-syms=N frame=black) replaces an agent tail-scraping 150 log lines. Pairs with task-142 (dedup+demote) and task-143 (digest). During this session's Celeste bring-up, every wall handoff cost an agent a big log tail; this one-liner would have made each near-free. Proven stop-reasons to encode from the session: missing-symbol (marker), unmapped-addr+VMA, unknown-instr, host-SIGSEGV-in-driver-.so (RADV, from a garbage Vulkan handle -> point at last CreatePipeline), spirv-val-fail.

2026-07-20 — AC #5..#10 landed (symbol attribution + HLE breadcrumb). Driver: Celeste dies on CLIMB (menu -> gameplay) with `UnmappedMemory (read) of 0x0` at `vmovdqa (%rax),%xmm2`, four frames deep in the guest's own libc — the signature of an optimised string/mem routine handed a NULL by one of our stubs.

- `ModuleManager::nearest_symbol` (crates/loader/src/manager.rs): address -> `module!symbol +offset`, largest export at/below. Retail exports are keyed by NID, reversed through `ps4_syscalls::SyscallId::from_nid`; an unrecognised NID prints as `NID <nid>`, never a guessed name. Exports carry no `st_size` (SceSym drops it), so it is "largest export <= addr", not a containment test.
- Wired into the existing fault annotator in app main (`try_read` on the module map so a thread faulting under the write lock still gets a report, not a deadlock). Every rbp frame and the faulting RIP go through it.
- `report_unmapped_memory` now annotates the faulting CODE (rip) as well as the faulting address; for a null deref the address is 0 and describing it says nothing.
- `ps4_core::breadcrumb` (new): always-on, per-thread `thread_local!` ring of the last 32 HLE calls — id/name, 6 register args, return value. Pushed before dispatch and patched after, so a handler that faulted/wedged shows as in-flight. No allocation on the dispatch path; dumped by `format_fatal` on the faulting thread.
- Not done: `report_exception` (#UD/#GP/int3) still annotates only the trapping instruction, not the module/symbol — same one-line treatment would apply if a later wall needs it.

Unverified observation from reading the HLE layer (NOT a diagnosis — the next run should name the culprit directly): pointer-returning paths that can hand the guest a literal 0 are the dlsym trap stub (crates/libs/src/lib.rs, `DLSYM_TRAP_MARKER` -> 0 for any unresolved symbol), `sce_kernel_dlsym` leaving `*func_out` untouched when the stub page can't be mapped (libkernel/mod.rs:264), `sceKernelGetProcParam` -> 0 (libkernel/mod.rs:60), and `sceAjmStrError` -> NULL `const char*` (libsceajm/mod.rs:148).

Update 2026-07-23 — missing-symbol reporter now resolves suffixed NIDs to names. The FATAL/log NID→name lookup in linker.rs (both the int-0x44-stub warn and the missing-symbol register) fed the full import `symbol_name` (`NID#library#module`, e.g. `bzQExy189ZI#W#W`) straight to `SyscallId::from_nid`, but the generated table is keyed by the BARE NID hash — so every suffixed import missed the table and printed a raw hash even when the name was known. Added `nid_key()` (strips at the first `#`) so the reporter reads `_init_env [NID bzQExy189ZI#W#W]` instead of `bzQExy189ZI#W#W`. Test `nid_key_strips_library_module_suffix`. Also widened the name corpus: `data/ps5_names.txt` (community wordlist, filtered) now feeds the same build.rs NID generation as `data/ps4_names.txt`, adding the PS5-only exports (`sceAgc*` etc.) so PS5 imports name too; a name only enters the table when our own clean NID hash reproduces the import's NID (self-verifying — the list is a dictionary, not an authority). Surfaced while bringing up the PS5 title Dead Cells (task-233).
<!-- SECTION:NOTES:END -->
