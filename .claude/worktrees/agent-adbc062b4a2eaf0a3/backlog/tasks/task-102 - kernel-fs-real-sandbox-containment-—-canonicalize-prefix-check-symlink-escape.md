---
id: TASK-102
title: >-
  kernel/fs: real sandbox containment — canonicalize + prefix-check (symlink
  escape)
status: Done
assignee: []
created_date: '2026-07-13 10:03'
updated_date: '2026-07-13 19:44'
labels:
  - security
  - filesystem
dependencies: []
ordinal: 101000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-100 review (Finding 4): FileSystem::resolve/translate reject the literal '..' component textually but do NOT resolve host symlinks. A symlink under game_data/app0 (shipped in a game dir or left by a prior guest write) lets unlink/rename/open follow it outside the sandbox at the OS level. Threat model is trusted homebrew so this is hardening, not urgent. Fix: canonicalize the resolved host path and verify it is still prefixed by the mount root before performing the op; reject otherwise.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 resolve() output is canonicalized and prefix-checked against the mount host root; a symlink pointing outside the sandbox is rejected
- [x] #2 Legitimate in-sandbox paths (incl. non-existent files being created) still work; 6 examples match baselines
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add contain(): canonicalize the resolved host path (resolving symlinks) and prefix-check against the canonical mount root; reject on escape. Canonicalize only the deepest existing ancestor + re-append the non-existent tail so file creation still works. Apply in translate() and the /app0 fallback. Symlink-escape + create unit test; 6 baselines.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-13. contain() canonicalizes resolved paths + prefix-checks vs canonical mount root; deepest-existing-ancestor trick keeps file creation working. Symlink-escape rejected (unit test), in-sandbox open+create work, 6 example baselines match.
<!-- SECTION:NOTES:END -->
