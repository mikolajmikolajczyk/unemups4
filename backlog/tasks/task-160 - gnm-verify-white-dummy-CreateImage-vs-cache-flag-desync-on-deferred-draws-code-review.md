---
id: TASK-160
title: >-
  gnm: verify white-dummy CreateImage vs cache-flag desync on deferred draws
  (code-review)
status: To Do
assignee: []
created_date: '2026-07-17 11:32'
labels:
  - gnm
  - gpu
  - review
  - bug
dependencies: []
priority: medium
ordinal: 166000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review flagged a possible desync: ResourceCache::get_white_dummy (crates/gnm/src/cache/mod.rs) pushes CreateImage+UploadImage into the cmd list AND caches white_dummy=Some(id) on first mint (called from crates/gnm/src/exec.rs bind_texture). If the FIRST white-dummy use is on a draw that subsequently DEFERS and the executor discards that draw's partially-built cmds, the CreateImage is dropped but the cache flag persists — a later draw calls get_white_dummy, sees the cached id, emits NO CreateImage, and binds an image the backend never created (Vulkan error/garbage). Confirm how the executor handles a deferring draw's already-pushed cmds: if they can be discarded while the cache retains white_dummy=Some(id), the mint must be tied to the same lifetime as the cmds (or emitted eagerly at cache init). If deferred draws never discard already-emitted cmds, close as not-a-bug with a note.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Determined whether get_white_dummy's CreateImage can desync from the white_dummy cache flag on a deferred draw; fixed or closed-with-rationale
<!-- AC:END -->
