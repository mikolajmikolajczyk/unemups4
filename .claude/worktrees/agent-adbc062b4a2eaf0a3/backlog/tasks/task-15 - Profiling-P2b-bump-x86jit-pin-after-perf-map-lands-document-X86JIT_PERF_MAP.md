---
id: TASK-15
title: 'Profiling P2b: bump x86jit pin after perf-map lands + document X86JIT_PERF_MAP'
status: Done
assignee: []
created_date: '2026-07-10 09:03'
updated_date: '2026-07-10 17:49'
labels:
  - profiling
dependencies:
  - TASK-12
priority: medium
ordinal: 15000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up to x86jit task-196 (env-gated perf-map emission, filed in x86jit backlog). Once that lands in x86jit and user merges it: bump both rev pins and document the JIT-visible perf workflow. After this, one flamegraph shows the full split: jit_0x... guest blocks vs x86jit_core::interp vs cranelift compile vs HLE handlers vs Vulkan present. BLOCKED until x86jit task-196 is merged (external dependency — x86jit changes go via its own backlog, never edited directly from here).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Cargo.toml rev pins for x86jit-core + x86jit-cranelift bumped to the commit containing perf-map support; cargo update -p x86jit-core -p x86jit-cranelift run
- [x] #2 commands.md Profiling section extended: X86JIT_PERF_MAP=1 perf record -g --call-graph dwarf -F 997 -- target/release/unemups4 <elf>
- [x] #3 perf report on a JIT-mode run shows jit_0x... guest-block symbols alongside host symbols; UNEMUPS4_BACKEND=interp run shows interpreter symbols instead
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Bump x86jit pin to post-perf-map main rev (decision-2 procedure), rebuild+test+oracle, document X86JIT_PERF_MAP in backlog/docs/commands.md profiling section.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
BLOCKED 2026-07-10 (Claude, feat/profiling). READ-ONLY inspection of /home/mikolaj/src/x86jit: main HEAD=d3de235; NO perf-map / X86JIT_PERF_MAP commit exists on any ref (git log --all grep = none). The upstream dependency x86jit backlog task-196 (Env-gated perf-map emission for JIT-compiled blocks, X86JIT_PERF_MAP=1) is still status: To Do — not implemented, not merged. Per the profiling-track plan and decision-2, this task must NOT be implemented until task-196 lands in x86jit main and the maintainer merges it. NOT STARTED here (no Cargo.toml pin bump, no cargo update, no doc change) — doing so would be a no-op or break the build. NEXT: when x86jit task-196 is merged, bump BOTH rev= pins in Cargo.toml to the perf-map commit, run 'cargo update -p x86jit-core -p x86jit-cranelift', rebuild + re-test, then extend commands.md with the X86JIT_PERF_MAP=1 perf workflow (AC#2) and verify jit_0x... block symbols appear (AC#3). x86jit remains read-only from here — its changes go through its own backlog.

RESOLVED 2026-07-10 (Claude). x86jit task-196 (perf-map, commit 1e839d6) merged into x86jit main at 23a35280c1f31788e3a7686c369c5d11f5e92ccf (merge of feat/perf-map). Bumped BOTH rev= pins (x86jit-core, x86jit-cranelift) 6cccf64 -> 23a3528 in Cargo.toml + `cargo update -p x86jit-core -p x86jit-cranelift` (AC#1). Extended backlog/docs/commands.md Profiling section with a "JIT perf-map (X86JIT_PERF_MAP)" subsection (perf record cmd, jit_0x... resolution, interp fallback, /tmp/perf-<pid>.map append-only) + updated intro/caveat (AC#2).

SCOPE SURPRISE (maintainer note): the old pin 6cccf64 was well behind main, so 6cccf64..23a3528 pulls in MUCH more than the assumed "perf-map + task-195 SIMD lift". Notably it adds Exit::PortIo (task-198, in/out trap-out) and threads CpuMode through disassemble() (2->3 args) + BlockKey. These are API-breaking, so a minimal compile-adaptation in crates/cpu/src/exec.rs was unavoidable: (a) add a fatal-diagnostic arm for Exit::PortIo in format_fatal (the run loop already routes it to Fatal via the `other =>` catch-all — no new semantics), and (b) pass CpuMode::Long64 to the two diagnostic disassemble() call sites (PS4 guest is a 64-bit PIE). No runtime/guest-behavior change.

VERIFICATION: `cargo build --release` green; `cargo clippy --all-targets --all-features -- -D warnings` clean; `cargo test` green (all suites pass); `cargo fmt` applied. Oracle `./scripts/run_examples.sh check`: only divergence is the documented headless Vulkan-driver ERROR line (exactly 4 added lines, all the same env quirk; 0 removed) — NO guest-output shift from the task-195 SIMD lift. Bonus perf-map file check (substantiates AC#2/#3 without perf): `X86JIT_PERF_MAP=1 UNEMUPS4_BACKEND=jit ./target/release/unemups4 examples/ps4-softgpu/ps4-softgpu.elf` produced /tmp/perf-<pid>.map with 11 well-formed `jit_0x<guest_rip>` block lines (`<hex host_addr> <hex size> jit_0x...`, 0 malformed). (helloworld exits before any block crosses the JIT hotness threshold, so it emits no map — use a heavier example.)

AC#3 NOT verified (left unticked): this env has no `perf` binary and kernel.perf_event_paranoid=2 (>1). Maintainer steps to close AC#3: `sudo sysctl kernel.perf_event_paranoid=1`; `cargo build --release`; `X86JIT_PERF_MAP=1 perf record -g --call-graph dwarf -F 997 -- target/release/unemups4 examples/ps4-softgpu/ps4-softgpu.elf`; `perf report --no-children` -> expect `jit_0x...` guest-block symbols alongside host frames; then `UNEMUPS4_BACKEND=interp perf record ...` -> expect `x86jit_core::interp::*` instead. The perf-map file check above already confirms the map x86jit emits is well-formed and PID-named, which is exactly what perf consumes.

2026-07-10 AC#3 CLOSED (maintainer, live over ssh, headless): perf 7.1.3 from the devShell (flake now ships pkgs.perf), paranoid=1. JIT run: perf report resolves `jit_0x400140` via the emitted /tmp/perf-<pid>.map (`[JIT] tid` DSO marker). Interp run (UNEMUPS4_BACKEND=interp): `x86jit_core::interp::interpret_block` symbols instead, no jit_0x. Both clauses of AC#3 verified; AC ticked. Note: `perf record` default event `cpu/cycles/P` fails on this laptop PMU — use `-e cycles:u` (or `-e task-clock` as software fallback).
<!-- SECTION:NOTES:END -->
