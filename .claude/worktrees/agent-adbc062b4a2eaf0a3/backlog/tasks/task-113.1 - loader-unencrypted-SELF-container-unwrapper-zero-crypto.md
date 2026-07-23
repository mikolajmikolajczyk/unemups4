---
id: TASK-113.1
title: 'loader: unencrypted SELF container unwrapper (zero crypto)'
status: Done
assignee: []
created_date: '2026-07-14 08:27'
updated_date: '2026-07-14 14:47'
labels:
  - retail
  - loader
dependencies: []
parent_task_id: TASK-113
priority: high
ordinal: 113000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
FASE 0 of the retail bring-up (parent epic). Retail native binaries (host eboot + native .prx) ship as SELF containers (magic 4F153D1D). The dumper produces UNENCRYPTED SELF (console-side decryption); unemups4 adds ZERO crypto — it only PARSES the container. Add a loader front-end: detect magic (7F454C46 -> pass through to goblin unchanged; 4F153D1D -> unwrap). Parse the SCE/SELF header + segment table, extract the plaintext ELF (ELF header + program headers + segment bytes) and hand it to the existing goblin path. NO decryption, NO signature check. Boundary: the copyrighted binary stays local + gitignored, never committed; the regression fixture is a SYNTHETIC minimal SELF, not game data.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 magic detection routes raw ELF unchanged and a SELF container into the unwrapper
- [x] #2 an unencrypted SELF unwraps to a byte-exact PIE ELF that goblin parses (program headers + segment offsets/sizes round-trip)
- [x] #3 a synthetic minimal-SELF fixture drives a unit test; no game asset is committed
- [x] #4 a signed/encrypted SELF is rejected with a clear error, never decrypted
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-14. DONE. KEY FINDING: the SELF unwrapper already existed (crates/loader/src/container.rs) — magic dispatch (ELF passthrough / SELF unwrap / unknown reject), SCE header + segment-table parse, blocked-bit (flags>>11) + phdr-id (flags>>20 &0xFFF) decode, inner-ELF reconstruction from blocked segments, compressed reject (comp!=uncomp), encrypted reject (inner magic != ELF). It is WIRED into the load path (image.rs:306, kernel/process.rs:60, hle.rs:204) and had a synthetic fake-SELF fixture (build_fake_self) + 12 unit tests.

My contribution: verified on the REAL first-target dump. Generalized the ignored real-dump smoke test (real_dumps_extract_inner_elf) to iterate a list of local paths incl. CUSA11302. All present modules unwrap + goblin-parse: eboot.bin -> 42MB inner ELF e_type=0xFE10 (ET_SCE_DYNEXEC) 11 phdrs; scePlayStation4/libfmod/libfmodstudio/libc/libSceFios2 .prx -> e_type=0xFE18 (ET_SCE_RELEXEC/PRX) 9-10 phdrs. Zero compressed blocked segments in Celeste modules (comp==uncomp), so the compressed-reject path is untouched for this title.

AC1 magic routing: done (open() + tests). AC2 unwrap->parseable byte-exact: synthetic round-trip + real CUSA11302 verified. AC3 synthetic fixture no-commit: done. AC4 encrypted/signed reject never decrypt: done (ContainerError::Encrypted). 46/46 loader tests, clippy+fmt clean. Dumps are local /home/mikolaj/PS4/, outside repo (never committed).
<!-- SECTION:NOTES:END -->
