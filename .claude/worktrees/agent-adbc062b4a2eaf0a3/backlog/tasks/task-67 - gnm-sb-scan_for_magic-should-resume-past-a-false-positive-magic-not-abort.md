---
id: TASK-67
title: 'gnm/sb: scan_for_magic should resume past a false-positive magic, not abort'
status: Done
assignee: []
created_date: '2026-07-11 18:17'
updated_date: '2026-07-13 19:37'
labels:
  - gpu
  - gnm
dependencies: []
priority: low
ordinal: 66000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable Runda-1 review NIT #4. crates/gnm/src/shader/sb.rs:425-445 — a stray 'OrbShdr' byte sequence inside the GCN code region makes parse_sb return LengthMismatch for the whole shader instead of resuming the scan past the bad candidate. Probability is tiny (7 exact bytes) but the fix is small: on LengthMismatch (or any per-candidate validation failure), continue scanning from candidate+1 until MAX_SCAN_BYTES is exhausted, only returning MagicNotFound/LengthMismatch when no candidate validates.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 a blob with a false 'OrbShdr' before the real header parses to the real shader (unit test)
- [x] #2 a blob with only a false magic and no valid header still rejects cleanly
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Make scan_for_magic resumable (start_off param); parse_sb loops over candidates, resuming one byte past a false 'OrbShdr' on validation failure until the real header validates or the scan window is exhausted. Extract validate_sb_candidate. On exhaustion, surface the last candidate error (LengthMismatch) or MagicNotFound. Add AC#1/#2 unit tests.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-13. scan_for_magic gained a start_off; parse_sb loops candidates, resuming past a false magic until the real header validates. AC#1 (false magic before real header parses to real shader) + AC#2 (false-only magic rejects) unit-tested; 16/16 sb tests pass.
<!-- SECTION:NOTES:END -->
