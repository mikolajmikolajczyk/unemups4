---
id: TASK-234
title: >-
  GNM: derivation ignores PS-interpolation registers (SPI_PS_INPUT_ENA/ADDR,
  SPI_PS_IN_CONTROL, SPI_BARYC_CNTL)
status: To Do
assignee: []
created_date: '2026-07-23 18:54'
labels: []
dependencies: []
ordinal: 239000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Surfaced by task-183 register-coverage audit. The GNM->pipeline derivation routes PS interpolants only via SPI_PS_INPUT_CNTL_n (state.rs gcn_ref_from_regs). The HLE shader-setup emitter (pm4/emit.rs) writes SPI_PS_INPUT_ENA, SPI_PS_INPUT_ADDR, SPI_PS_IN_CONTROL and SPI_BARYC_CNTL into the shadow register file, but no derivation reads them back, so PS interpolant enable/count/barycentric mode are ignored. Benign for the embedded corpus and current titles (fixed shaders); a correctness gap for real GCN pixel shaders in retail bring-up. Semantics: Mesa src/amd/registers/gfx6.json (SPI_PS_INPUT_ENA/ADDR, SPI_PS_IN_CONTROL, SPI_BARYC_CNTL). Audit + enforcing test live in crates/gnm/src/pm4/opcodes.rs; see backlog GFX6 register audit doc.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 SPI_PS_INPUT_ENA/ADDR interpolant masks feed PS input derivation
- [ ] #2 SPI_PS_IN_CONTROL NUM_INTERP consumed
- [ ] #3 SPI_BARYC_CNTL interpolation mode consumed or documented safe
<!-- AC:END -->
