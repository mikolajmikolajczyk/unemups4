---
id: TASK-235
title: >-
  GNM: derivation ignores PS export-format/kill registers
  (SPI_SHADER_COL_FORMAT, SPI_SHADER_Z_FORMAT, DB_SHADER_CONTROL)
status: To Do
assignee: []
created_date: '2026-07-23 18:54'
labels: []
dependencies: []
ordinal: 240000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Surfaced by task-183 register-coverage audit. Color format is derived from CB_COLOR0_INFO and depth presence from DB_Z_INFO/DB_DEPTH_CONTROL. The PS export-format registers SPI_SHADER_COL_FORMAT (per-MRT numeric export format), SPI_SHADER_Z_FORMAT (depth-export format) and DB_SHADER_CONTROL (Z_EXPORT/KILL/mask) are written by pm4/emit.rs into the shadow register file but no derivation reads them back. Benign for the RGBA8 videoout path; a correctness gap for float/HDR MRTs, PS depth export, and shader discard/alpha-to-coverage in retail bring-up. Semantics: Mesa src/amd/registers/gfx6.json (SPI_SHADER_COL_FORMAT, SPI_SHADER_Z_FORMAT, DB_SHADER_CONTROL). Audit + enforcing test live in crates/gnm/src/pm4/opcodes.rs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 SPI_SHADER_COL_FORMAT cross-checked against / used with CB_COLOR0_INFO
- [ ] #2 SPI_SHADER_Z_FORMAT + DB_SHADER_CONTROL Z-export/kill modelled or documented safe
<!-- AC:END -->
