---
id: TASK-126
title: 'libs: user-service event ring (replace one-shot LOGIN AtomicBool)'
status: Done
assignee: []
created_date: '2026-07-16 06:16'
updated_date: '2026-07-16 07:30'
labels:
  - from-code-review
  - libs
dependencies: []
ordinal: 132000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding (libsceuserservice get_event): the initial-user LOGIN is a single global AtomicBool one-shot re-armed unconditionally by Initialize. It models exactly one event (LOGIN), one user (USER_ID=1), never LOGOUT, and races: Initialize's store(false) can re-fire a spurious LOGIN for an already-active user, and compare_exchange hands the single login to exactly ONE of N polling threads (others see NO_EVENT; a poller expecting it blocks forever). Adequate for a single-threaded boot today; breaks on re-init, multi-poller, or dynamic login/logout. Fix: a small bounded per-user event ring drained FIFO, keyed by SceUserServiceUserId, seeded with the initial-user LOGIN.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a bounded FIFO event queue replaces the AtomicBool; initial-user LOGIN seeded once
- [x] #2 concurrent GetEvent pollers each drain distinct events; empty → NO_EVENT
- [x] #3 re-Initialize does not re-fire a LOGIN for an already-active user
<!-- AC:END -->
