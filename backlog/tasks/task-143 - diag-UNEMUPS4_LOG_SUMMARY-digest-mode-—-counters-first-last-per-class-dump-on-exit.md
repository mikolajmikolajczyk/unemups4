---
id: TASK-143
title: >-
  diag: UNEMUPS4_LOG_SUMMARY digest mode — counters + first/last per class, dump
  on exit
status: To Do
assignee: []
created_date: '2026-07-16 13:13'
labels:
  - diag
  - dx
dependencies: []
priority: medium
ordinal: 149000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
An alternative to streaming logs for agent-driven bring-up: instead of piping the whole stream and tail-ing it, accumulate a compact DIGEST and print it once on exit (or on a signal). Env lever UNEMUPS4_LOG_SUMMARY=1. The digest aggregates by event class: counts (N GNM submits, N dlsym misses with the distinct missing-symbol names, N unhandled-PM4 by opcode, N frames), and the first + last occurrence of each error/wall class (so an agent sees 'dlsym NOT-FOUND x74 [Graphics::GraphicsSystem::*, ...]; GNM submit x2; frame=black; stopped=RADV-... ' in a dozen lines instead of scrolling a 150-line tail). Complements task-113.2 (the why-did-it-stop one-liner) — 113.2 is the single stop-reason line, this is the per-class rollup. Implementation: a global aggregator behind the env flag that tracing events feed (or a dedicated summary sink); flush on Drop / on SIGINT/SIGTERM / at clean exit. Keep it off by default (zero overhead unless the flag is set). Agents read the digest, not the stream — big context saving.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 UNEMUPS4_LOG_SUMMARY=1 prints a compact per-class digest on exit/signal: counts per event class (submits, dlsym misses + distinct names, unhandled-PM4 by opcode, frames) + first/last of each error/wall class
- [ ] #2 Off by default (flag unset = zero digest overhead, current behavior)
- [ ] #3 A Celeste live-run with the flag yields a <~20-line digest capturing the same walls an agent would otherwise tail-scrape from 150 lines
<!-- AC:END -->
