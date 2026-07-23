---
id: TASK-14
title: 'Profiling P4: tracing spans on low-frequency paths + feature-gated Tracy layer'
status: Done
assignee: []
created_date: '2026-07-10 09:02'
updated_date: '2026-07-10 13:45'
labels:
  - profiling
dependencies:
  - TASK-13
priority: low
ordinal: 14000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Live timeline view in Tracy GUI (user-chosen viewer). Spans ONLY on low-frequency paths — never around cpu.run() slices (that split is task-13 counters). Non-enabled span = cached callsite check, records only when a span-consuming layer is active, so workspace crates emit spans unconditionally with no feature gate. Tracy wiring lives in the app crate behind a cargo feature so the default build is unaffected. Version lock-step between tracing-tracy crate and Tracy GUI handled by pinning the tracy package in the nix devShell (check the crate's compat table when adding). Fallback documented (not built): tracing-chrome -> ui.perfetto.dev for headless offline traces.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Spans added: rust_syscall_handler (crates/libs/src/lib.rs) debug_span!(syscall, id); display.rs RedrawRequested outer debug_span!(frame) + children fence_wait/acquire/fb_copy/record_submit/present; thread.rs spawned closure info_span!(guest_thread, tid); main.rs boot stages (HLE install, load_executable)
- [x] #2 app/unemups4/Cargo.toml feature profile-tracy = [dep:tracing-tracy]; init_logging() rewritten on Registry + .with(fmt_layer); under the feature adds tracing_tracy::TracyLayer
- [x] #3 flake.nix devShell gains tracy package, version-matched to the tracing-tracy/tracy-client crate protocol
- [x] #4 commands.md documents the Tracy workflow + tracing-chrome/Perfetto fallback note
- [x] #5 Default build unchanged (cargo tree -p unemups4 | grep -i tracy empty); cargo run --release --features profile-tracy + Tracy GUI shows frame lanes and syscall zones per guest thread live
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Add spans (unconditional, no feature gate — non-enabled span is a cached callsite check): rust_syscall_handler debug_span!(syscall,id) in libs/src/lib.rs; display.rs RedrawRequested outer debug_span!(frame)+children; thread.rs spawned closure info_span!(guest_thread,tid); main.rs boot stages. 2. app Cargo.toml feature profile-tracy=[dep:tracing-tracy]; rewrite init_logging() on Registry+fmt_layer, add TracyLayer under feature. 3. flake.nix devShell gains version-matched tracy. 4. commands.md Tracy + Perfetto fallback. 5. Default build unchanged (cargo tree | grep tracy empty). Live-Tracy verify not possible headless — note it.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Verified. AC#1 spans (unconditional, no feature gate in workspace crates): debug_span!(syscall,id) around HLE dispatch in libs/src/lib.rs; display.rs RedrawRequested outer debug_span!(frame) + children fence_wait/acquire/fb_copy/record_submit/present (guards dropped at each phase boundary so they nest correctly); thread.rs spawned closure info_span!(guest_thread,tid); main.rs boot stages info_span!(hle_install) + info_span!(load_executable). AC#2 app/unemups4/Cargo.toml feature profile-tracy=[dep:tracing-tracy] (tracing-tracy 0.11, optional, default=[]); init_logging split by cfg. AC#3 flake.nix devShell gains pkgs.tracy — VERSION-LOCKED: tracy-client-sys 0.28.0 (via tracing-tracy 0.11.4, resolved) speaks Tracy 0.13.1 protocol per the crate compat table, and nixpkgs-unstable ships tracy 0.13.1 (confirmed 'Tracy Profiler 0.13.1' on PATH). AC#4 commands.md Tracy workflow + tracing-chrome/Perfetto fallback note. AC#5 default build tracy-clean: 'cargo tree -p unemups4' has NO tracy; --features profile-tracy pulls tracing-tracy/tracy-client/tracy-client-sys. IMPORTANT DESIGN NOTE: tracing-subscriber 0.3.22 Full formatter always renders entered span scope, and the always-on spans would prepend 'span{fields}:' to every log line — diverging the oracle baselines. Fixed with a custom NoSpanFormat FormatEvent for the default (non-tracy) subscriber that reproduces the exact TIMESTAMP LEVEL ThreadId(NN) target: msg format WITHOUT the scope. Under --features profile-tracy the Registry+fmt+TracyLayer stack is used (spans do show in console there — acceptable for the opt-in profiling build). NOT verifiable headless: the live Tracy GUI showing frame lanes / syscall zones (AC#5 second clause) needs a running Tracy GUI + display session; the feature build compiles + the layer is wired, verified by inspection. Oracle default build: clean (only the known headless Vulkan line; no other +/- lines). clippy default + --all-features -D warnings clean; fmt clean; tests 9+3+7 green; profiler (task-13) still dumps correctly under NoSpanFormat, no panic.
<!-- SECTION:NOTES:END -->
