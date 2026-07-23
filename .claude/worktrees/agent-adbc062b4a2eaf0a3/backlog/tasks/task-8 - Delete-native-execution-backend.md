---
id: TASK-8
title: Delete native execution backend
status: Done
assignee: []
created_date: '2026-07-09 15:06'
updated_date: '2026-07-09 21:20'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-7
priority: medium
ordinal: 8000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Single execution path; dead code gone. Delete crates/cpu/src/{launch.s, trampoline.s, vmstate.rs} + global_asm! in cpu/lib.rs; delete crates/memory/src/linux.rs; remove arch_prctl remnants (kernel/thread.rs:47-51, libc::syscall(158,...)); remove set_trampoline/init_stubs trampoline fields in loader/linker.rs. Scrub README.md (documents native-HLE design and the "real syscall goes to host kernel" caveat — obsolete), AGENTS.md, backlog/docs/architecture.md and glossary.md (trampoline/FS-swap entries -> x86jit model).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 grep -rn 'cpu_launch_guest|syscall_trampoline|VmStateAbi|arch_prctl' crates/ app/ returns nothing
- [x] #2 cargo build --release green
- [x] #3 all 6 examples still match baselines
- [x] #4 README/AGENTS/architecture/glossary updated to describe x86jit execution model
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Delete crates/cpu/src/{launch.s,trampoline.s,vmstate.rs}; remove global_asm! + extern decls in lib.rs. 2. Delete crates/memory/src/linux.rs + pub use; fix vm_backend.rs doc-comment. 3. Remove arch_prctl remnants + VmState leftovers from kernel/thread.rs. 4. Sweep loader/linker.rs for trampoline leftovers. 5. Repo-wide grep sweep; clean/keep-history each hit. 6. Docs: README, AGENTS, architecture.md, glossary.md, status.md, dev-setup.md -> x86jit model. 7. Check cpu Cargo.toml/build.rs for dead asm plumbing. Verify: grep AC empty, cargo build --release + test green, run_examples.sh check 6/6.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Executed on branch chore/task-8-delete-native (from main), commit 6cc0404 (GPG-signed).

Deletions (4 files, 528 LOC): crates/cpu/src/launch.s (123), trampoline.s (93), vmstate.rs (178); crates/memory/src/linux.rs / LinuxMemoryManager (134). Removed global_asm! includes + cpu_launch_guest/syscall_trampoline extern block from cpu/lib.rs; removed pub use linux::LinuxMemoryManager + module decl from memory/lib.rs.

Note: kernel/thread.rs had NO live arch_prctl/libc::syscall(158,...) code (task-5/6 already replaced execution) and loader/linker.rs had NO trampoline fields (removed in task-5) — only stale comments. Reworded stale launch.s/trampoline.s/arch_prctl/VmStateAbi comment refs in cpu/exec.rs, cpu/guest_vm.rs, kernel/thread.rs, libs/pthread.rs, memory/vm_backend.rs, app/main.rs. No cpu build.rs / asm build plumbing existed (global_asm! was inline; nothing to remove from Cargo.toml).

Docs updated to x86jit model: README (how-it-works + obsolete host-kernel security caveat), AGENTS.md, backlog/docs/architecture.md (data-flow rewrite + layering note that ps4-cpu deps x86jit-core), glossary.md (reworked Trampoline->removed, FS base, Identity mapping; added x86jit, GuestVm, Exit::Syscall, HLT gadget). status.md/dev-setup.md had no stale execution claims. doc-1 migration plan left as history.

Verification: AC#1 grep (cpu_launch_guest|syscall_trampoline|VmStateAbi|arch_prctl over crates/ app/) returns NOTHING. cargo build --release green. cargo test workspace green (ps4-cpu run_guest 6/6, memory vm_backend 7/7, loader 3/3). ./scripts/run_examples.sh check = all 6 match baselines, exit 0. Left In Progress for coordinator merge.
<!-- SECTION:NOTES:END -->
