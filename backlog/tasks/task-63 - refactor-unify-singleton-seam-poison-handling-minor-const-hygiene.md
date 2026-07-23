---
id: TASK-63
title: 'refactor: unify singleton-seam poison-handling + minor const hygiene'
status: Done
assignee: []
created_date: '2026-07-11 14:47'
updated_date: '2026-07-11 15:31'
labels:
  - refactor
dependencies: []
priority: low
ordinal: 62000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Small seam/const hygiene (scoped tight — NOT a cross-crate framework). (a) register_present_sink (crates/core/src/gpu.rs:147-163) silently drops a poisoned write-lock while its stated twin register_kernel (crates/core/src/kernel.rs:101-114) logs error! — make consistent (add the matching arm, OR drop logging from both + document boot-single-threaded). (b) optional (D4): derive the libscegnmdriver test's expected NIDs from SyscallId::from_symbol_name rather than 17 hardcoded literals. NOTE: the driver()/bound_shaders() oncelock-macro dedup originally proposed here is MOOT — task-43 retires the bound_shaders() global, leaving driver() a lone singleton (no pair to macro). NON-GOAL: a cross-crate singleton macro for kernel/present/cpu seams (distinct policies, leave per 'no abstraction beyond need').
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 present_sink/kernel poison-handling consistent + documented
- [ ] #2 (optional) libscegnmdriver test derives expected NIDs instead of hardcoding
- [x] #3 tests + clippy + fmt + oracle green
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Unify poison-handling of the two RwLock<Option<Arc<dyn T>>> global-registration seams (register_present_sink in gpu.rs, register_kernel in kernel.rs). Pick shape (b): drop the error! logging from both write sides, add a one-line doc noting registration is boot-single-threaded so a poisoned lock can't occur. Keep read side (read().ok()? -> None) as-is. Touch ONLY gpu.rs + kernel.rs. Skip optional D4 (lives in libscegnmdriver, task-61 owns it). Macro dedup MOOT per task-43. Verify: build/test/clippy/fmt/run_examples.
<!-- SECTION:PLAN:END -->

## Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-11. Unified to shape (b): dropped the `error!` logging from
`register_kernel` (kernel.rs) and removed the now-unused `use tracing::error;`;
gave both seams an identical one-line doc noting registration is boot-single-
threaded (called once before guest threads start) so the write lock is
uncontended and can't be poisoned — a failed lock is silently ignored. Chose (b)
over (a) because it's the simpler uniform form and matches conventions.md
("validate only at system boundaries; trust internal code"): logging a
near-impossible poison at a boot seam is noise, not signal. The read side
(`read().ok()? -> None`) is unchanged in both files.

Edited spots:
- crates/core/src/kernel.rs — removed `use tracing::error;` (line 7); dropped the
  `else { error!(...) }` arm from `register_kernel`; added the boot-single-threaded doc.
- crates/core/src/gpu.rs — appended the matching boot-single-threaded sentence to
  `register_present_sink`'s doc (body was already the silent-drop shape).

D4 (deriving libscegnmdriver test NIDs) SKIPPED — out of scope: it lives in
crates/libs/src/libscegnmdriver/, owned by task-61 (splitting in parallel). AC#2
left unticked.

Macro dedup MOOT — the `oncelock_mutex_singleton!` idea is retired: task-43 drops
the `bound_shaders()` global, leaving `driver()` a lone singleton with no pair,
so no macro was built and driver()/bound_shaders() were left untouched.

Touched ONLY crates/core/src/gpu.rs + crates/core/src/kernel.rs.

Verify (worktree unemups4-t63, HEAD b0409e1):
- cargo build --release: 0 errors, 9 warnings (167 crates) — warnings are the
  pre-existing ps4-syscalls OpenOrbis-SDK-not-found build-script notices.
- cargo test: 116 passed, 3 ignored (25 suites).
- cargo clippy --all-targets --all-features -- -D warnings: exit 0.
- cargo fmt --check: exit 0.
- ./scripts/run_examples.sh check: all 6 examples match baselines (6/6).
<!-- SECTION:NOTES:END -->
