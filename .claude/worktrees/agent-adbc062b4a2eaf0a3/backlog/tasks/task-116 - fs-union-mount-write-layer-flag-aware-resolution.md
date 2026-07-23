---
id: TASK-116
title: 'fs: union-mount write-layer + flag-aware resolution'
status: To Do
assignee: []
created_date: '2026-07-14 20:18'
updated_date: '2026-07-14 20:33'
labels:
  - fs
  - bug
dependencies:
  - TASK-113.3
priority: medium
ordinal: 120000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding (task-113.3). FileSystem::translate() now unions overlapping /app0 mounts resolving purely by first-existing-file, IGNORING open flags. An O_CREAT|O_TRUNC or O_RDWR open of a name present only in the later title-dir mount resolves there and truncates/overwrites a shipped game asset instead of creating/writing a scratch file under game_data/app0. Fix: make the overlay explicit — a declared write layer vs read layers; O_CREAT/O_TRUNC/write intent resolves to the write layer, reads fall through existing-wins. Also: loader/process should own mount setup (it knows the exe path) instead of main.rs re-parsing argv.
<!-- SECTION:DESCRIPTION:END -->
