---
id: TASK-125
title: 'gnm: fetch-code terminator alignment check (guard early-truncation)'
status: Done
assignee: []
created_date: '2026-07-16 06:16'
updated_date: '2026-07-16 07:32'
labels:
  - from-code-review
  - gnm
dependencies: []
ordinal: 131000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding (shader/gcn.rs read_fetch_code): the fetch-shader window grows until decode_all sees ANY s_setpc/s_swappc, but a fetch shader has no declared length — a mid-instruction dword that decodes as SOP1 0x20/0x21 (inside an SMRD literal / buffer_load immediate) can stop growth EARLY. resolve_fetch_call/parse_fetch_shader then walk from offset 0 and splice a TRUNCATED body → the recompiled VS fetches fewer/wrong vertex attributes, no defer, no error logged (scrambled geometry). Fix: confirm the detected terminator lands at a decode position reached by a walk from offset 0 (instruction-aligned), not just anywhere in the byte window.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 read_fetch_code only accepts a terminator that a from-offset-0 decode actually reaches (aligned)
- [x] #2 a fetch body with a 0x20/0x21 byte pattern mid-instruction before the real return is not truncated
- [x] #3 unit test: a crafted misaligning window resolves the full fetch body or defers, never truncates silently
<!-- AC:END -->
