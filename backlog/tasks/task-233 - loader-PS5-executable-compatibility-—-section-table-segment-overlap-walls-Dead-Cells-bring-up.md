---
id: TASK-233
title: >-
  loader: PS5 executable compatibility — section-table + segment-overlap walls
  (Dead Cells bring-up)
status: Done
assignee: []
created_date: '2026-07-23 13:40'
updated_date: '2026-07-23 14:12'
labels:
  - loader
  - ps5
dependencies: []
priority: high
ordinal: 238000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
First unemups4->unemups5 loader step. Real PS5 native titles (PPSA01341 Demon's Souls, PPSA15552 Dead Cells) share the SELF container magic 0x1d3d154f with PS4 — magic is NOT a PS4/PS5 discriminator (empirically identical bytes 0x00-0x0B across CUSA and PPSA dumps). The real console tell is the imported GPU driver: PS4 links libSceGnmDriver/libSceGnm (GCN), PS5 links libSceAgcDriver/libSceAgc (RDNA2). No Platform flag needed — platform is emergent from the Agc import; keep it diagnostic-only.

Running Dead Cells (eboot.bin.esbak, the decrypted inner ELF) surfaced two concrete loader walls, both BEFORE any Agc GPU call:

Wall 1 — ELF parse. goblin rejects the image: e_shoff points past EOF (PS5 image carries a 48-entry section-header table pointer, section data stripped from the dump; e_shoff=0x318d0c8 > filesz 0x25d6d24). PS4 inner ELFs have shnum=0/shoff=0 so goblin is happy. The SELF eboot.bin fails the same way after reconstruction (bad offset 53143304).

Wall 2 — segment mapping. After neutralizing the section table (scratchpad hack), the SCE dynamic parse SUCCEEDS and reads the full PS5 import + needed-module list (incl. libSceAgcDriver.prx/libSceAgc.prx), then load fails with Error: Memory("Memory collision") during segment mapping. PS5 PT_LOAD layout overlaps under our mapper (e.g. ph[3] PT_LOAD and ph[4] type 0x6474e552 GNU_RELRO both at file offset 0x1a04000).

Scope: loader/container/image compatibility ONLY. NOT Agc GPU implementation. Goal: Dead Cells loads through both walls to its entry / first unimplemented-Agc stop, so the honest wall becomes 'Agc GPU not implemented' instead of a parse/map crash. Provenance: all facts derived from the real PPSA/CUSA dumps (forward-only); pin the Gnm-vs-Agc import discriminator with a witness test if a Platform label is ever added.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 PS5 image with e_shoff past EOF parses (section-header table tolerated/sanitized when out-of-bounds); PS4 images unchanged
- [x] #2 PS5 segment layout maps without 'Memory collision' (segment/RELRO overlap handled); the 3 CUSA PS4 titles + homebrew examples still load byte-identically
- [x] #3 Dead Cells (PPSA15552 esbak) loads past both walls and stops at the first unimplemented Agc/PS5 point, not a parse/map crash
- [x] #4 No Platform enum/Process.platform threaded; PS5-ness stays emergent from the Agc import (diagnostic naming only)
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Iterative smoke-loop bring-up on Dead Cells (PPSA15552 esbak), one wall at a time; verify PS4 no-regression after each.

Wall 1 (section-table). In image.rs, before goblin Elf::parse: if e_shoff != 0 and e_shoff + e_shnum*e_shentsize > raw.len(), the section-header table is stripped/out-of-bounds — sanitize by zeroing e_shoff/e_shnum/e_shstrndx in a mutable copy (goblin then parses phdrs only; our loader already ignores sections for mapping — it uses phdrs). Cite: ELF64 Ehdr field offsets (FreeBSD 9 sys/elf64.h) e_shoff@0x28, e_shentsize@0x3a, e_shnum@0x3c, e_shstrndx@0x3e. Guard so PS4 (shnum=0) path is byte-identical. Applies to both RawElf and reconstructed-SELF paths.

Wall 2 (Memory collision). Reproduce, then locate: extract_segments maps PT_LOAD (1) + PT_SCE_RELRO (0x61000010). PS5 also carries GNU_RELRO (0x6474e552) overlapping a PT_LOAD at the same file offset/vaddr. Decide: (a) skip GNU_RELRO (0x6474e552) — it's a protection view over an already-mapped PT_LOAD, not a distinct load (matches how PT_GNU_RELRO works on Linux), or (b) make the mapper's collision check merge/allow a RELRO sub-range of an existing mapping. Verify which segment pair collides via phdr dump; prefer (a) minimal. Confirm memory_size span calc still correct.

Verify: run PPSA15552 esbak -> must pass both walls, reach entry/first Agc stop. Regression gate: the 3 CUSA eboots (03173/05952/11302) + examples/*.elf load identically (stdout diff). cargo test --workspace green.

Delegate implementation to an opus subagent per-wall; main loop orchestrates + runs the regression checks. No commit without user request.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Root causes CONFIRMED from real dumps (2026-07-23):

WALL 1 — section table past EOF. PS5 esbak: e_shoff=0x318d0c8 (51957960), shnum=48, but filesz=0x25d6d24 (39677220) -> shdr table stripped. goblin Elf::parse reads section headers -> 'bad offset'. PS4 inner ELFs: shnum=0/shoff=0 (no shdr table) -> fine. All PS5 phdrs are in-bounds (max end == filesz). Fix: sanitize e_shoff/e_shnum/e_shstrndx to 0 in a mutable copy before goblin when e_shoff+shnum*shentsize > len (image.rs, ELF64 Ehdr offsets FreeBSD 9 sys/elf64.h: e_shoff@0x28, e_shentsize@0x3a, e_shnum@0x3c, e_shstrndx@0x3e). Our mapper uses phdrs only; sections are diagnostic. Same fix covers reconstructed-SELF path (eboot.bin failed identically, bad offset 53143304).

WALL 2 — sub-page abutting segments. PS5 Dead Cells PT_LOADs pack tight:
  ph[7] vaddr=0x1a44000 memsz=0x828dc0 end=0x226cdc0 (ends mid-page)
  ph[8] vaddr=0x226cdc0 memsz=0x5f8c58            (starts in the SAME page 0x226c000)
Not a vaddr overlap — they share one physical page. Our per-page VMA map() rejects ph[8] because page 0x226c000 is already inserted by ph[7] -> 'Memory collision'. PS4 (Celeste) segments sit on separate pages with large gaps (0x0 / 0x2a00000 / 0x2c00000), never sharing a page. Fix: the VMA/segment map must tolerate a segment beginning inside a page already mapped by the previous segment — map only the not-yet-mapped page delta (still copy all segment bytes into the identity arena), OR coalesce the abutting range. Locate the collision check in the memory manager (VmMemoryManager map) / linker map_image. Keep PS4 unchanged.

Bring-up state: with WALL 1 hand-patched (scratchpad copy, e_shoff zeroed), SCE dynamic parse succeeds and reads the full PS5 import list incl. libSceAgcDriver.prx/libSceAgc.prx; load then dies at WALL 2. Next stop after WALL 2 unknown (expect relocation/module-load, then eventually first unimplemented Agc).

RESULT (2026-07-23, implemented + independently verified; NOT committed):
Both walls fixed in crates/loader only (no crates/memory edit — reused existing is_memory_free):
- WALL 1: image.rs ParsedImage::parse now runs sanitize_out_of_bounds_section_table(&mut raw) before goblin — zeroes e_shoff/e_shnum/e_shstrndx only when the shdr table is out of bounds (zero-copy no-op for PS4/homebrew shnum==0). Cited to FreeBSD 9 sys/elf64.h. +2 unit tests.
- WALL 2: linker.rs map_image maps each segment's page span via new map_free_pages() that skips pages an earlier segment already claimed (coalescing free runs). PS4 gapped segments -> one map == old size (byte-identical). +1 unit test.
Verified independently: PS5 Dead Cells (raw esbak) clears BOTH walls -> 259861 relocations -> Executable loaded 0x400070 -> Thread 1 jumps -> executes guest -> stops at NEW honest wall: missing symbol NID 'bzQExy189ZI#W#W' (an unresolved PS5 libkernel import, suffix #W#W). PS4 CUSA11302/03173/05952 + examples byte-identical (no bad-offset/collision; relocations normal). cargo test --workspace 623 passed; clippy -D warnings clean on ps4-loader; fmt clean on touched files.
LOOSE END (benign, from WALL 2): one 'WARN protect on an untracked region 0x266cdc0' — RELRO re-protect now targets a mid-page vaddr that is no longer a VMA start because tight PS5 segments share a page. Non-fatal, load proceeds. Only surfaces now that tight PS5 segments load at all. Worth a follow-up if PS5 bring-up continues.
NEXT (separate work, new walls — not this task): resolve/alias PS5 libkernel NID 'bzQExy189ZI#W#W' and the imports after it; the Agc/RDNA2 GPU path is the real far wall.
<!-- SECTION:NOTES:END -->
