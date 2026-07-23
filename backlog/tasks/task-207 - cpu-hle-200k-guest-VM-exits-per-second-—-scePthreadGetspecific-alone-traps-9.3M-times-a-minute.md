---
id: TASK-207
title: >-
  cpu/hle: 200k guest VM exits per second — scePthreadGetspecific alone traps
  9.3M times a minute
status: To Do
assignee: []
created_date: '2026-07-21 18:28'
labels:
  - cpu
  - hle
  - perf
dependencies: []
priority: medium
ordinal: 212000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured on Celeste over 60 s of steady state: 12.08M syscall dispatches, i.e. roughly 200k VM exits per second. The dominant callers are not I/O but Mono's per-access thread-local and lock primitives — scePthreadGetspecific 9.3M calls (avg 60 ns each), scePthreadMutexUnlock 1.21M, scePthreadMutexLock 0.99M, scePthreadMutexTrylock 70k.

The handlers themselves are already cheap; the cost is structural. Every one of these is a full x86jit Exit::Syscall round trip out of JIT-compiled code, through the run loop, into the Rust dispatcher and back. The per-call handler time the profiler reports (60 ns) does NOT include that exit/entry overhead, which is attributed to guest exec instead — so the true cost of this traffic is understated by the current tables.

This is explicitly NOT the current bottleneck and must not be worked before the GPU path is fixed: the same measurement puts 77% of the frame inside sceGnmSubmitAndFlipCommandBuffers (task-203/204/205/206). File this so the finding is not lost — it is what will surface once the frame budget actually approaches 16.6 ms.

Directions worth evaluating when the time comes, cheapest first:
- measure the real exit/entry round-trip cost, so the decision rests on a number rather than on the call count looking large
- serve the hottest handlers without leaving guest execution (a guest-side fast path for TLS get, or an uncontended-mutex fast path), falling back to the syscall only on the slow path
- note per the standing rule that any x86jit-side change is filed in the x86jit backlog and landed by the maintainer, never edited directly from this repo
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the exit/entry round-trip cost of one guest syscall is measured, so the total cost of this traffic is a number and not an inference from the call count
- [ ] #2 the hottest handlers (scePthreadGetspecific, uncontended mutex lock/unlock) are evaluated for a path that avoids the full VM exit, with the chosen approach recorded
- [ ] #3 not started before the GPU-path tasks land; measured frame-rate effect recorded in the notes
<!-- AC:END -->
