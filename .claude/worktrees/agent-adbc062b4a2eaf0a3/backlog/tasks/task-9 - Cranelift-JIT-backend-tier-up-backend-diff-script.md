---
id: TASK-9
title: Cranelift JIT backend + tier-up + backend diff script
status: Done
assignee: []
created_date: '2026-07-09 15:06'
updated_date: '2026-07-10 07:06'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-8
priority: medium
ordinal: 9000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Native-speed execution; interpreter kept for debugging. crates/cpu: x86jit-cranelift dep; backend selection via env var UNEMUPS4_BACKEND=interp|jit (one binary, no feature matrix); JitBackend::new(), set_tier_up_after(~50), set_tier_up_background(true); verify tier-up handle lifecycle (wait_idle) against x86jit-cli usage. SMC already safe (all embedder writes route through vm.write_bytes). Guard pages: NOT now (arena pre-mapped RW; out-of-span access SIGSEGVs like native today) — file follow-up task. New scripts/diff_backends.sh: run every example under both backends, diff stdout.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 all 6 examples match baselines with JIT backend
- [x] #2 scripts/diff_backends.sh diffs clean interp vs jit
- [x] #3 hello-world wall time under JIT within ~2x of old native run (sanity, not hard gate)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
## Implementation Plan (task-9)

1. crates/cpu/Cargo.toml: add `x86jit-cranelift.workspace = true` (root already
   declares the git+rev pin; default feature `jit` pulls cranelift).
2. GuestVm::new: select backend from env `UNEMUPS4_BACKEND` (`jit` default | `interp`).
   - jit: `Box::new(JitBackend::new())` + `vm.set_tier_up_after(Some(50))` +
     `vm.set_tier_up_background(true)` before the Arc (both are `&mut self`, called
     alongside the existing pre-map).
   - interp: `Box::new(InterpreterBackend)` (unchanged).
   - Log one info line with the chosen backend at startup.
   - Lifecycle: no explicit wait_idle/drain — `Vcpu::run` drains bg tier-up
     internally (vm.rs drain_tier_up); JitBackend::Drop joins the worker. GuestVm
     owns the Vm which owns the boxed backend, so Arc drop cleans up. wait_idle is
     a test-only determinism lever (CLI never calls it in production).
   - Guest arena stays RWX pre-mapped RW (reserve_at); JIT compiles into its own
     JITModule exec arena and only reads/writes guest RAM via host_base+addr, so no
     W^X concern. guest_base flows through materialize() already.
3. scripts/diff_backends.sh: source run_examples.sh's normalize/strip_noise, run each
   example under UNEMUPS4_BACKEND=interp and =jit, diff normalized output, nonzero on
   mismatch. Fix run_examples.sh EXIT-trap `tmp: unbound variable`.
4. Verify: run_examples.sh check under jit (default) + interp 6/6; diff_backends clean;
   time hello_world jit vs interp.
5. ps4-thread-testing 10x under jit — counter 40000 stable.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
## Backend wiring (crates/cpu)

- `crates/cpu/Cargo.toml`: added `x86jit-cranelift.workspace = true` (root already
  declares the git+rev pin; the crate's `default = ["jit"]` feature pulls cranelift).
- `GuestVm::new` selects the backend from `UNEMUPS4_BACKEND` (`jit` default | `interp`,
  one binary, no feature matrix). Unset/empty/`jit` → JIT; `interp` → interpreter; any
  other value → warn + default to JIT.
  - JIT: `Box::new(JitBackend::new())` (native host target — matches x86jit-cli's plain
    `EngineKind::Jit` arm).
  - Tier-up config (mirrors x86jit-cli's `TIER_UP_AFTER = 50` + `TierUp::Background`):
    `vm.set_tier_up_after(Some(50))` + `vm.set_tier_up_background(true)`. Both are
    `&mut self` on `Vm`, so they run before the `Arc` wrap alongside the existing `map`.
    Applied only for the JIT (interpreter is `Unsupported` for bg tier-up and would just
    degrade to inline).
  - Lifecycle: NO explicit `wait_idle`/`tier_up_handle` needed. `Vcpu::run` drains
    completed background compiles itself (x86jit-core `vm.rs::drain_tier_up`, called on
    every `resolve` when `tier_up_background`), and `JitBackend::Drop` signals shutdown +
    joins the compiler worker when the `Vm`'s `Arc` is released. `wait_idle` is a
    test-only determinism lever in x86jit — the CLI never calls it in production either.
  - Startup log: one `info!("guest execution backend: {kind:?} (via UNEMUPS4_BACKEND)")`.

## Guard pages / arena (in scope check)

- Deferred per doc-1 dec 5 / task-9 — arena stays pre-mapped RW via `reserve_at`; nothing
  extra required. The JIT compiles guest blocks into its own `JITModule` executable arena
  and touches this RW guest RAM only through baked `host_base + guest_addr` inlined
  accesses, so the pre-mapped-RWX guest arena needs no W^X setup. `guest_base` already
  flows through `Backend::materialize(..., guest_base)`. No engine gap surfaced.

## scripts

- New `scripts/diff_backends.sh`: runs every example under `UNEMUPS4_BACKEND=interp` and
  `=jit`, diffs the two NORMALIZED outputs against each other (interp is the JIT's oracle),
  nonzero exit on any mismatch. Reuses run_examples.sh's `strip_noise`/`normalize`/
  `run_one`/example arrays by SOURCING it — run_examples.sh now guards `main` behind a
  `BASH_SOURCE == $0` check so sourcing only imports helpers. shellcheck-clean (0.11.0).
- EXIT-trap fix (task-8 quirk): run_examples.sh's `do_check` set `trap 'rm -f "${tmp}"'
  EXIT` where `tmp` was a function `local`; the EXIT trap fires at *shell* exit — after
  the function returned — so under `set -u` it aborted with `tmp: unbound variable`
  (which also masked the real exit code). Fixed: use a script-global `CHECK_TMP` (pre-init
  `""`) with a `[[ -n ... ]]` guard in the trap. Check now exits 0 cleanly.
- The new "guest execution backend: ..." startup line is stripped in `strip_noise` (a
  config marker, not guest behavior, and necessarily differs interp-vs-jit) — so the
  pre-task-9 baselines stay valid under both backends and diff_backends compares clean.

## Verification results

- `./scripts/run_examples.sh check` (JIT default): 6/6 OK, exit 0.
- `UNEMUPS4_BACKEND=interp ./scripts/run_examples.sh check`: 6/6 OK, exit 0.
- `./scripts/diff_backends.sh`: 6/6 interp == jit, exit 0.
- `cargo test -p ps4-cpu` (runs under default JIT): 6/6 pass. clippy clean.

## Thread stability under JIT (bg tier-up race area)

- ps4-thread-testing 10x under JIT: 10/10 PASS, "Counter Final: 40000" every run.
  Background tier-up compilation racing guest threads is stable; no fallback to foreground
  tier-up needed.

## Perf sanity (hello_world wall time, 3 runs each, warm)

- interp: 0.007 / 0.009 / 0.008 s
- jit:    0.008 / 0.008 / 0.010 s
- hello_world's guest work is trivial (a few printf syscalls); wall time is dominated by
  process/loader startup, not guest execution, and blocks rarely reach tier_up_after=50 —
  so interp and JIT are indistinguishable (~8ms). Well within the plan's "~2x of old
  native" aspiration (no native path remains to compare against directly; native was
  deleted in task-8). No hard gate — recorded as sanity.

## x86jit changes

- None. No `Exit::UnknownInstruction` appeared only under JIT and no interp-vs-jit
  behavioral divergence surfaced across all 6 examples + the 10x thread run. No pin bump.
<!-- SECTION:NOTES:END -->

## Post-merge datapoint (2026-07-10, user-verified on live display)

ps4-softgpu renders at **34 FPS under the JIT** vs **5 FPS under the interpreter**
(the task-7 screenshot run predated JIT wiring) — ~7x speedup on real guest
rendering. hello_world remains startup-dominated (~8 ms both backends).
Related fix: flake.nix devShell now sets LD_LIBRARY_PATH (wayland, libxkbcommon,
vulkan-loader) so winit's runtime dlopen works inside `nix develop` without a
manual `LD_LIBRARY_PATH=/usr/lib`.
