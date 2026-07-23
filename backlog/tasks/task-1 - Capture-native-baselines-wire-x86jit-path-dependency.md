---
id: TASK-1
title: Capture native baselines + wire x86jit path dependency
status: Done
assignee: []
created_date: '2026-07-09 15:05'
updated_date: '2026-07-09 16:30'
labels:
  - migration
  - x86jit
dependencies: []
priority: high
ordinal: 1000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Freeze current native-execution behavior as the migration oracle; make x86jit available to the workspace. Add workspace path deps: x86jit-core and x86jit-cranelift = { path = "../x86jit/..." } (unused yet). New scripts/run_examples.sh: run each of the 6 example ELFs (examples/ps4-helloworld/hello_world.elf, ps4-fs.elf, ps4-mmap.elf, ps4-tls.elf, ps4-thread-testing.elf, ps4-softgpu.elf) with a timeout, capture stdout+exit code into scripts/baselines/*.txt, normalize nondeterminism (TIDs/timestamps) with sed. Commit baselines. Full plan: backlog/docs/the x86jit CPU backend
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 scripts/run_examples.sh produces baselines for all 6 example ELFs
- [x] #2 re-running the script diffs clean against committed baselines (determinism)
- [x] #3 cargo build --release green with x86jit path deps added
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Branch chore/task-1-baselines from main. 2. Add x86jit-core + x86jit-cranelift path deps to root Cargo.toml [workspace.dependencies] (unused; adopted in task-3). 3. Write scripts/run_examples.sh (bash, shellcheck-clean) with capture+check modes, timeout 30s per ELF, sed-normalize nondeterminism (TIDs/addrs/timestamps), softgpu timeout-kill expected. 4. Capture baselines, run check x2 for determinism. 5. Commit Cargo.toml/lock + script + baselines (GPG-signed, conventional). 6. Tick ACs + notes.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE (branch chore/task-1-baselines; leave status In Progress until merge).

WIRING: added x86jit-core + x86jit-cranelift as path deps (../x86jit/...) in root [workspace.dependencies]. Not referenced by any member crate yet (adopted in task-3), so — as the task's worst case anticipated — Cargo.lock is UNCHANGED (cargo only locks workspace deps a crate actually uses). Resolvability proven: 'cargo metadata' exits 0 (paths parse/exist) and full-workspace 'cargo build --release' is GREEN (rc=0, 0 errors; pre-existing rust-2024 warning[E0133] lints in ps4-gpu only). AC#3 met.

ORACLE: scripts/run_examples.sh with capture|check modes (shellcheck-clean). Builds once via cargo, then runs target/release/unemups4 directly under 'timeout 30s' (cargo run would spend the timeout recompiling the vulkan/winit stack). Captures combined stdout+stderr -> strip_noise -> normalize (ANSI strip; mask <TS>/ThreadId(N)/<PID>/<TID>/<ADDR>/<DUR>, guest tid=/key=/Thread N, repo path -> <REPO>) -> 'sort -u'. Baselines in scripts/baselines/*.txt with per-example header documenting the compromises. AC#1 met (all 6). AC#2 met: ran capture, then check 8x back-to-back -> 8/8 clean; also proved the oracle FAILS on a tampered baseline then passes after restore.

NONDETERMINISM COMPROMISES:
- HLE lib load order + main/guest thread interleave -> absorbed by sort.
- The winit/display MAIN thread panics in this devShell (no usable wayland/XKB), which (a) is host-env noise and is stripped, and (b) makes the PROCESS exit code race between 0 (guest exit(0) wins), 101 (panic wins) and 124 (softgpu timeout). So exit code is NOT asserted; the guest's real end is captured as the '[SYSCALL] exit(0)' log line. (libwayland is made discoverable via LD_LIBRARY_PATH so the guest still runs to completion in most examples.)
- ps4-thread-testing: genuine thread-scheduling nondeterminism (worker TIDs assigned in scheduler order; a join-completion tail line intermittently truncated by the panic race). COMPROMISE: guest tids masked, sort -u collapses duplicate-count jitter, and the racy pthread-join tail (Joining thread/joined successfully/scePthreadJoin) is filtered. Baseline asserts the SET of distinct masked events.

SOFTGPU: runs (boot log + VideoOut register-buffer + double-buffering guest lines captured); window loop never returns so the 30s timeout kills it (expected). No display could open (devShell wayland/XKB), so it panics on the main thread like the others — pre-window boot log is the oracle. AC#1 counts it as captured.

Left In Progress per instructions (Done only after user merges to main).
<!-- SECTION:NOTES:END -->
