---
id: TASK-142
title: 'diag: log hygiene — dedup/coalesce tracing Layer + demote per-call info->debug'
status: To Do
assignee: []
created_date: '2026-07-16 13:13'
labels:
  - diag
  - dx
  - tech-debt
dependencies: []
priority: medium
ordinal: 148000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Emulator logs are a firehose that eats agent context during live-run debugging. Two source-level fixes: (1) a custom tracing Layer that COALESCES consecutive identical (or same-callsite) log lines into one with a repeat count — e.g. 'Kernel: tls_set_specific tid=1 key=10 ... (x342)' instead of 342 lines. tracing has no built-in dedup; a small fmt-layer wrapper keeps a last-line hash + counter and flushes '(xN)' when the line changes or on a time/threshold. Biggest raw win — the firehose is mostly exact repeats (tls_set_specific, repeated dlsym NOT-FOUND, per-frame state). (2) AUDIT info! callsites that fire per-syscall/per-frame and DEMOTE them to debug!/trace! — info should mark milestones (module load, GNM submit, wall reached), not per-call chatter. Known offenders: ps4_kernel::bridge tls_set_specific (fires hundreds of times at info), per-symbol sceKernelDlsym NOT-FOUND warns, per-call bridge logs. Keep the milestone info lines. Net effect: RUST_LOG=warn,ps4_gnm=info becomes terse enough that 2>&1 is readable without aggressive tail. Relates to [[png-visual-oracle]] discipline (logs lie; keep them small so the signal survives). No behavior change beyond log verbosity.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A tracing Layer coalesces consecutive identical/same-callsite lines into one '(xN)' line; verified on a Celeste live-run (the tls_set_specific / dlsym-miss floods collapse to counted single lines)
- [ ] #2 Per-syscall/per-frame info! callsites demoted to debug!/trace! (tls_set_specific + audited others); milestone info (module load / GNM submit / wall) preserved
- [ ] #3 RUST_LOG=warn,ps4_gnm=info on a Celeste run produces a readable stream (order-of-magnitude fewer lines) with no loss of signal (walls/errors still visible)
<!-- AC:END -->
