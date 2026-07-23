---
id: TASK-81
title: >-
  core: unify the four registered-singleton globals into one generic + RAII
  scoped test override
status: Done
assignee: []
created_date: '2026-07-12 07:55'
updated_date: '2026-07-12 08:45'
labels:
  - core
  - gpu
dependencies: []
priority: low
ordinal: 80000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable phase-4 quality review finding #3 (QUALITY). Four hand-rolled singleton registries now exist: register_kernel (crates/core/src/kernel.rs:104), register_dirty_source (dirty.rs:32-49), register_present_sink (gpu.rs:162-180), register_bounded_read (bounded_read.rs:49-77). HONEST READ: the CONVENTIONS are consistent and healthy (every global degrades to None, callers degrade safely, identical poison handling, cross-referenced docs) — the proliferation is NOT a design smell, each is a distinct seam. The DEBT is the MECHANISM: four copies of RwLock<Option<Arc<dyn T>>> + register/getter/(clear) + identical doc boilerplate, and each new global re-invents test serialization — shader_bind.rs needed a module-wide TEST_LOCK (shader_bind.rs:274) + manual clear_bounded_read() choreography where a panic between register and clear leaves the global wired and breaks unrelated headless-path tests in the same process. FIX: one small generic in ps4-core, e.g. Registered<T: ?Sized> with register()/get() and — gated on the test-hooks feature — override_scoped(v) -> Guard (RAII that serializes on an internal per-slot mutex and restores the prior value on drop, panic-safe). Port the four globals to it; TEST_LOCK + clear_bounded_read disappear. WHY: phase 4 will likely add a 5th global (shader/pipeline cache or coherence source) — make it a one-liner, not a 5th boilerplate copy + 4th TEST_LOCK.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 a generic Registered<T> (register/get + test-hooks override_scoped RAII guard) exists in ps4-core; the 4 existing globals are ported to it, behavior + degrade-to-None semantics unchanged
- [ ] #2 TEST_LOCK (shader_bind.rs) and clear_bounded_read are removed; the affected tests use the RAII scoped override (panic-safe, no cross-test bleed)
- [ ] #3 build/test/clippy/fmt green; no change to boot registration order or headless behavior
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-81 @<prior-history>, merged). NEW Registered<T: ?Sized> (crates/core/src/registered.rs): const fn new, register(Arc<T>), get()->Option (degrade-to-None, poison-tolerant, never panics); test-hooks-gated override_scoped(Arc<T>)->ScopeGuard + override_none_scoped + reset. Panic-safe+serialized: override_inner locks per-instance Mutex (poison-tolerant via into_inner), mem::replace slot, stash prior in ScopeGuard which holds MutexGuard for life + restores prior in Drop (runs on panic → global can't stay wired for unrelated test). 4 globals ported behavior-identical (same pub names/sigs as thin wrappers, no external caller changed, main.rs boot order untouched): kernel, dirty_source, present_sink (only that registry; BackendCmd/GpuBackend untouched), bounded_read. shader_bind.rs TEST_LOCK removed → 5 headless tests override_none_scoped, wired-path override_scoped; bounded_read unit test → override_scoped. FLAGGED: kept clear_bounded_read as thin wrapper over Registered::reset because exec.rs (task-82's file) test calls it — honored the do-not-edit-exec fence; can drop once exec.rs moves to the guard. Verify: workspace 187 pass, clippy 0, fmt clean. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
