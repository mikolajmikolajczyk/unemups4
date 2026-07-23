---
id: TASK-114
title: 'flaky: ps4-thread-testing intermittent host SIGSEGV on thread teardown'
status: To Do
assignee: []
created_date: '2026-07-14 19:39'
labels:
  - bug
  - threading
dependencies: []
priority: low
ordinal: 118000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ps4-thread-testing.elf occasionally (~1 in 20) dumps core (host SIGSEGV) on the JIT backend. Timing-sensitive race in thread teardown/detach cleanup; does NOT reproduce under gdb (15 clean runs) and passes 5/5 direct runs typically. Surfaced during retail bring-up (task-113.3) when a grep started including 'dumped core'. Not deterministically tied to the session's mutex/cond HLE changes but investigate those (mutex-object write, cond no-op) + thread.rs detached-cleanup race. Low priority — rare, homebrew-only.
<!-- SECTION:DESCRIPTION:END -->
