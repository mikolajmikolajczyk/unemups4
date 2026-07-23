---
id: TASK-13
title: >-
  Profiling P3: aggregate profiler — UNEMUPS4_PROFILE counters + periodic/atexit
  dump
status: Done
assignee: []
created_date: '2026-07-10 09:02'
updated_date: '2026-07-10 13:45'
labels:
  - profiling
dependencies:
  - TASK-12
priority: medium
ordinal: 13000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Headless-friendly quantitative time split (guest exec vs HLE syscalls vs GPU present), no perf privileges needed. Gate: env var UNEMUPS4_PROFILE=1 or =<secs> (dump interval, default 10) — house style like UNEMUPS4_BACKEND/UNEMUPS4_WATCHDOG, no CLI flag. Design constraint: high-frequency paths get relaxed AtomicU64 counters, never tracing spans; everything behind a once-resolved enabled() bool so the default path pays one branch. cpu.run() returns ~once per syscall (budget None default) so two Instant::now() + two fetch_add per slice is negligible. Reads x86jit counters via existing pub API (vm.cache.*, vm.backend.compile_ns(), Vcpu::fast_hits()) — zero x86jit changes, current pin. Details in plan file phase 3 (3a/3b/3c).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 New crates/cpu/src/profile.rs: enabled() (OnceLock<bool> from UNEMUPS4_PROFILE), static ExecStats (AtomicU64 relaxed: guest_ns, run_slices, syscall_ns, syscall_count, exits_budget/hlt/fatal, vcpu_fast_hits), PER_SYSCALL Mutex<HashMap<u64, IdStat{count,ns}>>, snapshot() -> Snapshot
- [x] #2 drive() in crates/cpu/src/exec.rs instrumented: enabled() resolved once at entry; Instant pairs around cpu.run(...) (:251) and dispatch(id, &mut ctx) (:284); exit-kind counters in match arms; cpu.fast_hits() accumulated on return paths
- [x] #3 crates/gpu/src/display.rs PresentStats (static atomics: frames, fence_wait_ns, acquire_ns, fb_copy_ns, record_submit_ns, present_ns, pace_sleep_ns + snapshot()) timed around existing present phases; gpu duplicates the tiny env read (no new cross-crate dep)
- [x] #4 app/unemups4: profiler_dump.rs dump thread (holds Arc<GuestVm>) prints table via info!(target: unemups4::profile) every interval: wall time, guest exec ns+%, syscall ns+%, slices, exit histogram; top ~15 syscalls by total ns named via ps4_syscalls::SyscallId::as_str(); x86jit counters (cache hits/misses/chained/ibtc_filled/regions/tier_bg_published/tier_bg_rejected, backend.compile_ns(), fast_hits); GPU frames + avg ms per phase per frame
- [x] #5 libc::atexit final dump registered (std::process::exit runs atexit handlers, so end-of-run table survives the guest exit path)
- [x] #6 UNEMUPS4_PROFILE=1 cargo run --release prints tables; UNEMUPS4_BACKEND=interp shows guest_ns up + compile_ns 0; profile off leaves scripts/run_examples.sh baseline unchanged; clippy -D warnings clean
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. crates/cpu/src/profile.rs: enabled() OnceLock<bool> from UNEMUPS4_PROFILE (=1 or =secs), static ExecStats atomics + PER_SYSCALL map, Snapshot. Add GuestVm accessors for x86jit cache/backend counters (vm field is private). 2. Instrument drive() in exec.rs: resolve enabled() once, Instant pairs around cpu.run and dispatch, exit-kind counters, fast_hits accumulate. 3. gpu display.rs PresentStats atomics timed around present phases; duplicate env read. 4. app profiler_dump.rs: dump thread holding Arc<GuestVm>, info!(target: unemups4::profile) table every interval; atexit final dump via libc::atexit. 5. Verify: PROFILE=1 prints tables, interp shows compile_ns 0; off => oracle unchanged; clippy clean.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Verified. AC#1 crates/cpu/src/profile.rs: enabled() OnceLock, ExecStats atomics, PER_SYSCALL Mutex<HashMap>, snapshot(). AC#2 drive() in exec.rs: profiling resolved once, Instant pairs around cpu.run + dispatch, exit-kind counters, fast_hits folded once at the single loop exit (loop refactored to break-with-value). AC#3 crates/gpu/src/present_profile.rs PresentStats timed around fence_wait/acquire/fb_copy/record_submit/present/pace_sleep in display.rs; gpu duplicates the env read (no ps4-cpu dep). NOTE: present rows only emit on a real GPU session — headless has no frames, so AC#3 verified by code inspection + gated build, not a live present. AC#4 app/unemups4/src/profiler_dump.rs: periodic dump thread (info! target unemups4::profile) + full table (wall, guest/syscall ns+%, exits, top-15 syscalls named via SyscallId::as_str, x86jit counters, gpu present avg ms/phase). AC#5 libc::atexit final dump — IMPORTANT: atexit runs after TLS teardown so tracing panics with AccessError; the final dump writes to stderr via eprintln! instead (build_dump() returns Vec<String>, no TLS). Confirmed no panic. AC#6 UNEMUPS4_PROFILE=1 prints tables (jit: sceKernelDebugOutText x6, x86jit hits/misses); interp shows guest_ns + compile_ns=0; profile OFF => oracle only the known headless single-line Vulkan divergence, no other +/- lines; clippy -D warnings clean; fmt clean; cargo test 9+3+7 green.
<!-- SECTION:NOTES:END -->
