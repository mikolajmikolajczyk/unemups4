---
id: TASK-183
title: >-
  gnm: audit which pipeline-affecting GFX6 registers the derivation still
  ignores
status: Done
assignee: []
created_date: '2026-07-20 12:30'
updated_date: '2026-07-23 19:00'
labels:
  - gnm
  - registers
  - audit
dependencies: []
priority: medium
ordinal: 187000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-179 hit FOUR registers that exist in hardware, change results, and were never read: CB_TARGET_MASK (fixed in e060725), CB_SHADER_MASK, CB_COLOR_CONTROL, and SPI_PS_INPUT_CNTL (the task-179 root cause). Two were even named in derive.rs doc comments as inputs to the blend derivation while the code ignored them, and one was emitted by our own PM4 emitter yet never consumed. Discovering these one wall at a time is expensive — each cost hours of misdirected investigation. Do a deliberate sweep instead: enumerate the GFX6 context/SH registers that affect rasterisation, blending, shader I/O routing and target state, and check each against what derive.rs actually reads. Produce a list of ignored-but-significant registers with a judgement on each (model it, or record why it is safe to ignore).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A written inventory of pipeline-affecting registers vs what the derivation reads
- [x] #2 Each ignored register is either modelled or has a recorded reason it is safe to ignore
- [x] #3 Registers defined in opcodes.rs but read nowhere are flagged, since that gap is what hid three of the four
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Audit approach: (1) enumerate every pipeline-affecting register const in opcodes.rs reg module (CONFIG/CONTEXT/SH/UCONFIG bases + offsets) + reg_name resolver = authoritative defined-set; (2) map which of those derive.rs actually reads (the derivation) = read-set; (3) defined-minus-read = ignored-set; classify each ignored reg via CLEAN oracle (AMD GCN ISA / Mesa src/amd radeonsi / Linux AMD headers) as either MODEL-IT (pipeline-affecting, gap) or SAFE-TO-IGNORE (with cited reason). Deliverables: inventory as a backlog doc + an enforcing coverage TEST in gnm that fails when a reg const is defined but neither read by derive nor on an explicit reasoned allow-list (this is AC#3's anti-drift mechanism — the gap that hid 3 of 4). No commit; opus subagent implements, main loop gates.
<!-- SECTION:PLAN:END -->

## Notes

Done 2026-07-23. Deliverables:
- Inventory: `backlog/docs/doc-8 - GFX6-pipeline-affecting-register-audit-—-derivation-coverage.md`.
- Catalog + anti-drift test: `crates/gnm/src/pm4/opcodes.rs` — `reg::ALL_PIPELINE_REGS` (42 regs; `reg_name` now resolves scalar names from it) and test `pipeline_register_coverage_is_audited` partitioning it into `READ_BY_DERIVATION` (27) and `IGNORED_WITH_REASON` (15); fails naming any unclassified register.
- Read-nowhere flagged (AC#3): `SPI_SHADER_PGM_RSRC3_PS/VS`, `CB_COLOR0_VIEW` — all benign, reasoned in the allow-list.
- GAP follow-ups filed: task-234 (PS-interpolation regs), task-235 (PS export-format/kill regs). No pipeline modelling changed (scope: audit + test).
- Verify green: build, `cargo test -p ps4-gnm` (270 passed), clippy clean, fmt clean. No commit.
