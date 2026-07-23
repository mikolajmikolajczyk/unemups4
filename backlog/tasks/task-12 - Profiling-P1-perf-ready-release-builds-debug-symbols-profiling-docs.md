---
id: TASK-12
title: 'Profiling P1: perf-ready release builds (debug symbols) + profiling docs'
status: Done
assignee: []
created_date: '2026-07-10 09:01'
updated_date: '2026-07-10 13:45'
labels:
  - profiling
dependencies: []
priority: medium
ordinal: 12000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
First layer of the profiling stack (plan: profiling for unemups4). Make 'cargo build --release' binaries consumable by Linux perf/flamegraph/hotspot, and document the profiling workflow. No runtime cost — debug info only affects binary size and link time. JIT'd guest code will show as [unknown] until the x86jit perf-map task lands (separate task).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Workspace Cargo.toml has [profile.release] debug = true (leave [profile.dev.package.ps4-syscalls] override alone)
- [x] #2 backlog/docs/commands.md gains a Profiling section: perf record -g --call-graph dwarf,16384 -F 997 -- target/release/unemups4 <elf>; perf report --no-children / hotspot; cargo flamegraph --release -p unemups4; note kernel.perf_event_paranoid <= 1 requirement and the JIT-code-[unknown] caveat
- [ ] #3 perf report on a helloworld run shows named frames for rust_syscall_handler, x86jit_core::interp, cranelift compile fns, and the Vulkan present path
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Add [profile.release] debug = true to workspace Cargo.toml (leave ps4-syscalls overrides alone). 2. Add Profiling section to backlog/docs/commands.md: perf record/report, cargo flamegraph, perf_event_paranoid note, JIT-[unknown] caveat. 3. Verify release build green + run helloworld under perf to confirm named frames (AC#3).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
AC#1/#2 verified: release profile emits DWARF (binary reports 'with debug_info', 'not stripped'); commands.md Profiling section added. AC#3 NOT executed: no 'perf' binary in this devShell/env and perf_event_paranoid=2 (needs root to lower). Verified the substance instead via 'nm target/release/unemups4': the required frames ARE present as named symbols — rust_syscall_handler (global T), x86jit_core::interp::* fns, cranelift/compiler compile paths, and Vulkan present-path symbols. So perf/flamegraph will resolve them on a host with perf + perf_event_paranoid<=1. Left AC#3 unchecked (couldn't run perf here). Oracle: only the known headless single-line Vulkan-driver divergence; no regression.
<!-- SECTION:NOTES:END -->
