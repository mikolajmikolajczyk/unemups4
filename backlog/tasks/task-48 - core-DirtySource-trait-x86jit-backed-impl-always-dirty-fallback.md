---
id: TASK-48
title: 'core: DirtySource trait + x86jit-backed impl + always-dirty fallback'
status: Done
assignee: []
created_date: '2026-07-11 12:55'
updated_date: '2026-07-11 17:46'
labels:
  - gpu
  - core
dependencies: []
priority: medium
ordinal: 47000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Promote DirtySource from gnm/cache stub to ps4-core (§8.3 home), wired at runtime via OnceLock/RwLock registration (like register_kernel/register_present_sink). Two impls: (a) in crates/cpu, over GuestVm's watch_range/unwatch_range/take_dirty_ranges (pinned rev 26bc5ec has them on Vm); (b) AlwaysDirty fallback (current conservative per-submit) when no VM wired (headless) or via env lever. Poll-and-drain at submit boundaries only (respects MemConsistency::Fast). Does NOT change cache behavior (P4-14 consumes); does NOT touch mprotect.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: AlwaysDirty + a mock impl unit-tested (watch→write-sim→drain)
- [x] #2 integration (headless-able): a crates/cpu test runs JIT'd guest code writing a watched range, asserts take_dirty_ranges reports it, unwatched writes don't
- [x] #3 registration mirrors kernel pattern; gnm reaches it without depending on cpu
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Confirm x86jit watch_range/unwatch_range/take_dirty_ranges signatures at pinned rev + reachability from GuestVm (crates/cpu). 2. Add DirtySource trait to ps4-core (crates/core/src/) with OnceLock/RwLock registration mirroring register_kernel/register_present_sink incl. boot-single-threaded poison doc. 3. x86jit-backed impl in crates/cpu over GuestVm. 4. AlwaysDirty fallback (headless/env lever). 5. Poll-and-drain at submit boundaries only. 6. Remove stub from gnm/cache/mod.rs, re-point to core (gnm must NOT depend on cpu). 7. AC#2 integration test in crates/cpu forcing JIT backend, proving JIT'd stores seen + unwatched not. 8. Verify build/test/clippy/fmt/run_examples 6of6 + cargo tree gnm no cpu.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
UNBLOCKED 2026-07-11: x86jit task-216 LANDED (x86jit HEAD bf63067 — JIT codegen/memory.rs now calls note_watched_store on store paths incl. rep-movs; task-216 Done, all ACs green) and the unemups4 rev pin was BUMPED 26bc5ec→bf63067 (build/test/clippy/fmt/oracle-6of6 green). watch_range/take_dirty_ranges now covers JIT'd stores, so this task's AC #2 CAN pass. Still verify AC #2 under the JIT backend (default) not just UNEMUPS4_BACKEND=interp. Original blocker context (for history):
BLOCKED (partial) by x86jit task-216 (filed 2026-07-11 in x86jit backlog @ dbc1e4f, UNLANDED). Fable phase-4 review finding #1: the Cranelift JIT (unemups4's DEFAULT backend, tier-up after ~50 execs) inlines guest stores raw with NO watch check, so watch_range/take_dirty_ranges is BLIND to JIT'd stores at the pinned rev 26bc5ec AND x86jit HEAD 47b7e6f. => THIS TASK's AC #2 (JIT'd guest code writing a watched range → take_dirty_ranges reports it) CANNOT PASS until x86jit-216 lands + the pin is bumped. Sequence: (a) land x86jit-216 (maintainer) + bump rev pin; (b) THEN implement the x86jit-backed DirtySource here and tick AC #2. Until then ship ONLY the AlwaysDirty fallback (correct but re-uploads every submit — task-49's 'clean-hit = 0 uploads' won't actually run live). Verify AC #2 under the JIT backend, not just UNEMUPS4_BACKEND=interp (interp would misleadingly pass).

DONE 2026-07-11 (feat/dirtysource @ e2176b0). DirtySource trait promoted to ps4-core `crates/core/src/dirty.rs`: `watch/unwatch/take_dirty` + `register_dirty_source`/`dirty_source()` global (RwLock<Option<Arc>>, boot-single-threaded-poison doc mirroring register_kernel/register_present_sink). Two impls: (1) real x86jit-backed `VmDirtySource` in `crates/cpu/src/dirty.rs` — forwards to GuestVm.vm().{watch_range,unwatch_range,take_dirty_ranges} (all &self, reachable via Arc<GuestVm>); cpu now deps ps4-core (no cycle; gnm still does NOT dep cpu — cargo tree empty). (2) `AlwaysDirty` fallback in core (reports every watched range every poll). Wired at boot in app/unemups4/src/main.rs right after GuestVm::new, env lever `UNEMUPS4_DIRTY=always` forces AlwaysDirty. gnm/cache/mod.rs stub removed, re-exports `ps4_core::dirty::DirtySource` (stayed inside cache/mod.rs only per fence). AC#1: core unit tests (MockDirty watch→sim-write→drain; AlwaysDirty; registration roundtrip). AC#2: `crates/cpu/tests/dirty_source.rs` runs `mov [rdi],eax; ret` under a NEW test-only `GuestVm::new_eager_jit_for_test` (set_tier_up_after(Some(0)) + background OFF): warmup run compiles the block, 2nd run executes JIT-compiled code (foreground tier-up returns the compiled block same-run; verified compile_ns>0 via temp probe), the inlined store into the watched page IS reported through VmDirtySource, an unwatched-page store is NOT, and drain empties. Test PANICS under UNEMUPS4_BACKEND=interp (no false green) — ran under DEFAULT (JIT) backend. Verify: cargo build --release OK; cargo test 120 passed/3 ignored (incl. dirty_source under JIT); clippy -D warnings EXIT 0; fmt --check EXIT 0; run_examples check 6/6. Did NOT change cache behavior (task-49 consumes), did NOT touch mprotect.
<!-- SECTION:NOTES:END -->
