---
id: TASK-117
title: >-
  kernel: run module_start as an explicit process bootstrap phase (ordering +
  failure)
status: To Do
assignee: []
created_date: '2026-07-14 20:18'
updated_date: '2026-07-14 20:33'
labels:
  - kernel
  - tech-debt
dependencies:
  - TASK-113.3
priority: medium
ordinal: 121000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review altitude finding (task-113.3). Dependency module_start invocation lives in Thread::execute()'s guest-run closure gated by is_main, entangling module lifecycle with thread lifecycle. Hazards: (a) a module_start that calls scePthreadCreate spawns a worker whose Thread::execute (is_main=false) skips module_inits and runs immediately, possibly before later leaves-first module_starts run or before main_thread_pthread is published -> race/null-deref; (b) a module_start returning GuestExit::Fatal is only logged and execution continues into the eboot CRT with half-initialized modules. Fix: Process/loader runs module_start bootstrapping as an explicit startup phase producing a ready entry; a Fatal aborts process bring-up; the thread just runs guest code.
<!-- SECTION:DESCRIPTION:END -->
