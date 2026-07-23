---
id: TASK-113.2.1
title: >-
  diag: reusable guest-module dump for offline disassembly
  (UNEMUPS4_DUMP_MODULES)
status: To Do
assignee: []
created_date: '2026-07-21 07:49'
updated_date: '2026-07-21 07:49'
labels: []
dependencies: []
parent_task_id: TASK-113.2
ordinal: 195000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Env-gated dump of loaded post-relocation module images (.bin) + .map sidecars so any loaded guest module can be disassembled/decompiled offline in objdump/Ghidra/radare2. Replaces ad-hoc one-time disassembly for guest-side crash investigation.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 UNEMUPS4_DUMP_MODULES=<dir> writes <name>.bin (loaded segment image, file offset N == VA base+N) and <name>.map (layout+sections+exports sorted by addr + objdump/Ghidra recipe header) for each non-HLE module
- [ ] #2 Reads via SMC-safe read_bytes_ranged seam; partially-unmapped range dumps readable bytes, zero-fills+notes gaps, never faults
- [ ] #3 Zero cost when env unset; dump dir gitignored; commands.md documents it
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented: crates/loader/src/dump.rs (maybe_dump_modules + format_map + read_module_image), hooked in app/unemups4/src/main.rs after load_executable, gitignore module-dumps/, commands.md section, 8 unit tests on .map format. FOLLOW-UP (deliberately NOT done): synthetic-ELF wrapper — emit a minimal ELF (single PT_LOAD at base_addr, e_machine=EM_X86_64, section+symtab from the .map) so objdump/Ghidra/r2 auto-detect arch+base and pick up export symbols, instead of the reader passing --adjust-vma/-m by hand. Do this only if the flat .bin+.map form proves awkward in practice.
<!-- SECTION:NOTES:END -->
