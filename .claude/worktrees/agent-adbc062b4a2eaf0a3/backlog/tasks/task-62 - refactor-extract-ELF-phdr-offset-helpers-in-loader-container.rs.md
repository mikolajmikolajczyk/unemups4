---
id: TASK-62
title: 'refactor: extract ELF phdr offset helpers in loader container.rs'
status: Done
assignee: []
created_date: '2026-07-11 14:47'
updated_date: '2026-07-11 15:12'
labels:
  - loader
dependencies: []
priority: medium
ordinal: 61000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/loader/src/container.rs unpacks ELF64 header/program-header fields from identical magic byte offsets in multiple sites: e_phoff/e_phentsize/e_phnum (0x20..,0x36..,0x38..) at :185-187/:271-273/:349-351; p_offset/p_filesz (0x08..,0x20..) at :215-216/:361-362. L1 deliberately avoids goblin (manual parse intentional) but the OFFSET constants shouldn't be re-typed per site — an error in one won't match another. Extract fn phdr_table_info(ehdr)->(u64,usize,usize) + fn phdr_extent(phdr)->(u64,u64) inside container.rs. Local, contained, no new cross-crate abstraction.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 phdr_table_info + phdr_extent extracted; all call sites use them (no remaining duplicated ELF offset literals in container.rs)
- [x] #2 loader tests green; SELF+ELF parse byte-identical (real Bloodborne eboot still unwraps)
- [x] #3 clippy + fmt clean
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Read container.rs, verify exact byte offsets + endianness at each of the 5 sites (:185-187,:271-273,:349-351 phdr table; :215-216,:361-362 phdr extent). 2. Add local helpers phdr_table_info(ehdr)->(u64,usize,usize) and phdr_extent(phdr)->(u64,u64) preserving exact offsets/endianness. 3. Replace all duplicated offset literals at call sites. 4. Verify: cargo build --release, cargo test (loader), clippy -D warnings, fmt --check, run_examples.sh check 6/6.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-11 (worktree unemups4-t62, branch cleanup/elf-helpers, not committed).

Sole file changed: crates/loader/src/container.rs. Behavior-preserving.

Two local helpers added (module-private, no cross-crate export, no goblin):
- fn phdr_table_info(ehdr: &[u8]) -> (u64, usize, usize) — returns (e_phoff @0x20 u64 LE, e_phentsize @0x36 u16 LE as usize, e_phnum @0x38 u16 LE as usize).
- fn phdr_extent(phdr: &[u8]) -> (u64, u64) — returns (p_offset @0x08 u64 LE, p_filesz @0x20 u64 LE).

Call sites updated (all duplicated ELF phdr offset literals removed; grep for the 4 magic ranges now matches only inside the two helper bodies):
- read_program_headers ehdr unpack (was :185-187) -> phdr_table_info.
- read_program_headers per-phdr extent (was :215-216) -> phdr_extent.
- extract_from_self phdr_region_end block (was :271-273) -> phdr_table_info; e_phoff cast to usize at the one call site that needs it (helper returns u64 per signature).
- tests::build_fake_self ehdr unpack (was :349-351) -> phdr_table_info.
- tests::build_fake_self per-phdr extent (was :361-362) -> phdr_extent.
Note: e_shoff@0x28 / e_shnum@0x3C writes in build_fake_self are section-header fields, out of scope, untouched.

Verification (all green):
- cargo build --release: 0 errors (only pre-existing ps4-syscalls SDK build-script warnings, unrelated).
- cargo test -p ps4-loader: 46 passed, 2 ignored.
- cargo clippy --all-targets --all-features -- -D warnings: 0 clippy lints (passes; the 9 warnings are ps4-syscalls build-script println!, environmental).
- cargo fmt --check: clean (exit 0).
- ./scripts/run_examples.sh check: all 6 examples match baselines.
- Real-dump oracle (ignored test real_dump_extracts_inner_elf against /home/mikolaj/PS4/CUSA03173/eboot.bin, read-only, never copied/committed): 1 passed — Bloodborne SELF still unwraps to a valid ELF with non-empty program headers, byte-identical.

Left uncommitted for maintainer.
<!-- SECTION:NOTES:END -->
