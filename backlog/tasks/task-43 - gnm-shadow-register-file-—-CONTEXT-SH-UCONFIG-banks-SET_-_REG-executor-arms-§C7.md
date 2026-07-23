---
id: TASK-43
title: >-
  gnm: shadow register file — CONTEXT/SH/UCONFIG banks + SET_*_REG executor arms
  (§C7)
status: Done
assignee: []
created_date: '2026-07-11 12:54'
updated_date: '2026-07-11 17:38'
labels:
  - gpu
  - gnm
dependencies: []
priority: medium
ordinal: 42000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Adds §5/§C7 RegFile (sparse index→u32, three banks) to state.rs; executor applies IT_SET_CONTEXT_REG/SET_SH_REG/SET_UCONFIG_REG (and SET_CONFIG_REG) bodies during run(), in every mode from PresentSubset up. Decide+implement state scoping: a per-Executor-instance GpuState threaded across submits via the driver (replacing "everything global"), with CONTEXT_CONTROL/CLEAR_STATE stubbed as full-clear. Offsets use existing reg_base consts. Foundation every derived view (P4-09/10/11) reads. Does NOT interpret any specific reg index yet; does NOT touch backend.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: a decoded SET_CONTEXT_REG multi-dword packet lands values at right absolute indices (base+offset+i) in right bank
- [x] #2 headless: IT_CLEAR_STATE resets banks; state persists across packets/submits until cleared
- [x] #3 headless: existing exec.rs present/sync/draw tests pass unchanged
- [x] #4 UNEMUPS4_PM4_TRACE output unaffected
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. state.rs: add RegFile (sparse index->u32 map per bank) + GpuState{ctx,sh,uconfig,config: RegFile, shaders: BoundShaders} with apply_set_reg(base,body), clear(). Keep BoundShaders. Delete BOUND_SHADERS global + free fns.
2. exec.rs: Executor<'a>{mode,sink,state:&'a mut GpuState}; new(mode,sink,&mut state). run() borrows &mut self; apply SET_*_REG bodies via GpuState in present_sync_on modes; IT_CLEAR_STATE resets banks; dispatch_draw_auto reads state.shaders. Update tests (no more BOUND_TEST_LOCK; per-test GpuState).
3. driver.rs: GnmDriver gets state: GpuState field + accessor; doc the lock invariant (display thread must never lock driver()).
4. shader_bind.rs: rewire binds to driver().lock().state.shaders.set(...).
5. submit.rs: build Executor with &mut drv.state() under the held lock.
6. Verify: build/test/clippy/fmt/run_examples/cargo tree.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
SCOPE CONFIRMED by pre-phase-4 boundary review 2026-07-11 (this is cleanup finding #5, the main state-scoping debt). Current: Executor is a STATELESS reader of a process-global — gnm/exec.rs:59-62 Executor{mode,sink} holds no GpuState; dispatch_draw_auto (exec.rs:112) reads the global static BOUND_SHADERS (gnm/state.rs:49 OnceLock<Mutex<BoundShaders>>), written by the HLE handler (libscegnmdriver/mod.rs:386,399 → bind_embedded_shader). Tests even need BOUND_TEST_LOCK to serialize around it (exec.rs:252) — a smell. This task MUST: (1) move BOUND_SHADERS off the global into an Executor-owned GpuState (the RegFile banks live here as fields, not a global — doc-2 §5 'not one giant struct every phase rewrites'); (2) change Executor::new + dispatch_draw_auto to take &mut state; (3) wire the HLE bind to write per-executor/per-submit state. WHY before the RegFile lands: multi-ring/multi-context submits (decision-4 §5 ring/queue ids) would all stomp one shared register file = correctness bug once two rings interleave + test-isolation nightmare. Migration surface = Executor struct + dispatch_draw_auto signature. driver() (gnm/driver.rs:114) stays a driver-lifetime singleton but its submissions should flow into the per-submit executor, not accumulate globally. PRESENT_SINK (core/gpu.rs:147) legitimately global — leave. Related cleanup: task-63 notes the driver()/bound_shaders() oncelock-macro is moot once this lands.

FABLE PHASE-4 REVIEW finding #2 (2026-07-11) — two caveats to bake in: (a) SEQUENCING: this task MUST fully land before task-44/45/46 (register binds / V# / RT derivation) start, or each will grow its own ad-hoc global like BOUND_SHADERS did. (b) LOCK INVARIANT: record_submit already holds driver().lock() across the whole exec.run(...) (libscegnmdriver/submit.rs:97-139), and exec.run blocks on the display channel (run_command_list/submit_and_flip both rx.recv(), gpu/lib.rs:29,62). Once GpuState lives in the driver, this lock-hold is LOAD-BEARING: the display thread MUST NEVER acquire driver() (instant deadlock), and every other guest thread's Gnm HLE call now serializes behind a present. Fine for the phase-4 corpus — but write this invariant into the code (doc comment on driver()/GpuState) so a later multi-thread change doesn't reintroduce the deadlock. Confirmed-feasible home: GpuState in GnmDriver (driver.rs:114 singleton), per-submit Executor borrows &mut.

---

DONE 2026-07-11 (worktree unemups4-p43, branch feat/gnm-regfile — NOT committed).

Shape landed (gnm/state.rs):
- `RegFile { regs: HashMap<u32,u32> }` — sparse absolute-index→u32 bank. set/get/len/is_empty/clear.
- `GpuState { ctx_regs, sh_regs, uconfig_regs, config_regs: RegFile, shaders: BoundShaders }`. Derived-view `BoundShaders` kept as a FIELD (doc-2 §5). Methods: `apply_set_reg(base, body)` (body = [offset, v0, v1…] → writes v_i at base+offset+i; empty/offset-only body = no-op), `clear_regs()` (all four banks; shader view left intact), `bind_embedded_shader(stage,id)`.
- HOME: `GnmDriver.state: GpuState` (driver.rs singleton) — persists across submits. Accessors `state_mut()`/`state()`.

Executor signature (gnm/exec.rs):
- `Executor<'a> { mode, sink, state: &'a mut GpuState }`; `new(mode, sink, &mut state)`; `run(&mut self, …)`.
- run() applies IT_SET_CONTEXT/SH/UCONFIG/CONFIG_REG → `state.apply_set_reg(set_reg_base(op), body)` and IT_CLEAR_STATE → `state.clear_regs()`, in every present_sync mode (PresentSubset up; TraceOnly still returns early). No register index interpreted (deferred to 44/45/46). dispatch_draw_auto reads `self.state.shaders`.

Retired the global:
- Deleted `static BOUND_SHADERS` + `bound_shaders()`/`bind_embedded_shader()`/`bound_shaders_snapshot()` free fns and the `BOUND_TEST_LOCK` test serialization + `reset_bound()`. Tests now use a per-test `GpuState` (no shared global → no serialization needed).
- HLE binds (libs/…/shader_bind.rs sceGnmSetEmbeddedVs/PsShader) now `driver().lock() → drv.state_mut().bind_embedded_shader(…)`.
- submit.rs builds the Executor with `drv.state_mut()` UNDER the already-held driver lock.

Lock-invariant doc: written on `driver()` (driver.rs) + module/`GpuState` docs (state.rs) + the submit.rs exec block — "the display thread must NEVER acquire driver() (instant deadlock)".

Verify (all green): cargo build --release (0 err); cargo test (128 passed, 3 ignored, 25 suites); cargo clippy --all-targets --all-features -D warnings (0 err — the 9 warnings are the ps4-syscalls SDK-not-found build-script notice, not code); cargo fmt --check (clean); ./scripts/run_examples.sh check (6/6 match baselines, incl. ps4-softgpu; ps4-pm4-test corpus mirror guarded by corpus_mirror_matches_opcodes test); cargo tree -p ps4-gnm | grep -iE 'ash|winit|vulkan' (empty). Embedded-draw behavior unchanged (exec draw tests pass as-is). ACs #1–#4 ticked.

Unsure/flagged: `apply_set_reg` uses `wrapping_add` for base+offset+i (guest offsets are small window-relative; wrapping just avoids a panic on a malformed packet — never fatal, consistent with decoder). CONFIG bank routed as the `_` fallback in `bank_mut` (only the 4 SET_*_REG windows reach it). IT_CONTEXT_CONTROL is NOT wired to clear (task said CLEAR_STATE resets; CONTEXT_CONTROL left as a plain skip — flag if it should also full-clear).
<!-- SECTION:NOTES:END -->
