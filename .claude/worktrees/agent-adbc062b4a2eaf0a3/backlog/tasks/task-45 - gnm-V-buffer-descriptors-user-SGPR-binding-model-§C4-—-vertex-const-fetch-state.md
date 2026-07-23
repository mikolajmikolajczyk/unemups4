---
id: TASK-45
title: >-
  gnm: V# buffer descriptors + user-SGPR binding model (§C4) — vertex/const
  fetch state
status: Done
assignee: []
created_date: '2026-07-11 12:54'
updated_date: '2026-07-12 15:58'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-43
priority: medium
ordinal: 44000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Descriptor-from-memory model: decode SPI_SHADER_USER_DATA_* SH regs as the user-SGPR block; given HostShader I/O layout (which user SGPRs hold V# pointers/descriptors), decode 128-bit V# buffer descriptors (base_addr, stride, num_records, dfmt/nfmt/swizzles) from guest memory into typed BufferDesc. Extends GpuState with vtx/const-buffer derived views (§5). Produces the (addr,size,layout) triples the cache (P4-14) consumes + vertex-input part of PipelineKey. Does NOT decode T#/S# (P4-20) or fetch shaders (P4-12); does NOT upload.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: hand-built user-data block + V# in mock memory → correct base/stride/records/format (units incl dfmt/nfmt table for corpus formats)
- [ ] #2 headless: draw-time derivation returns full set of referenced buffer ranges for a corpus-style draw
- [ ] #3 headless: malformed/null descriptors → clean per-draw defer, no crash
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Add ps4-gnm vbuf.rs: decode SPI_SHADER_USER_DATA_* user-SGPR block from SH regs; decode 128-bit V# (base/stride/num_records/dfmt/nfmt/swizzles) from guest mem via BoundedRead into typed BufferDesc; draw-time derivation producing (addr,size,ResLayout) triples for ResourceCache + vertex-input descriptor; malformed/null → clean per-draw defer. 3 headless unit-test ACs.
<!-- SECTION:PLAN:END -->
