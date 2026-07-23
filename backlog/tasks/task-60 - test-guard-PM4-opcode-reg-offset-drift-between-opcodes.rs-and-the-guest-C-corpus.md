---
id: TASK-60
title: >-
  test: guard PM4 opcode/reg-offset drift between opcodes.rs and the guest C
  corpus
status: Done
assignee: []
created_date: '2026-07-11 14:47'
updated_date: '2026-07-11 15:12'
labels:
  - gnm
dependencies: []
priority: high
ordinal: 59000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
PM4 IT_* opcodes + GFX6 reg-base/offset constants are mirrored in crates/gnm/src/pm4/opcodes.rs (source of truth) AND re-#define'd in examples/ps4-pm4-test/ps4-pm4-test/main.c:40-52 ('mirror opcodes.rs'). The C corpus is the VALIDATION INPUT for the Rust decoder — if a value diverges, the test still compiles+runs, silently tracing the wrong packet / mis-resolving a register window. Nothing enforces the mirror; phase-4 grows both tables. Add a cross-check: generate a pm4_opcodes.h from opcodes.rs (build-script / checked-in generated header the example #includes) OR minimally a Rust test asserting the shared IT_*/reg values. Keep small (handful of shared consts). Optional nice-to-have (D4): derive the libscegnmdriver test's expected NIDs from SyscallId::from_symbol_name instead of 17 hardcoded literals (mod.rs:593-610).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A check (generated header the C #includes, or an assertion test) fails if any shared IT_*/reg constant diverges between opcodes.rs and the guest corpus
- [x] #2 Single source of truth (opcodes.rs) documented
- [x] #3 existing PM4 trace/decode tests + oracle unchanged
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Add a #[cfg(test)] drift-guard in crates/gnm/src/pm4/opcodes.rs that at test-time reads examples/ps4-pm4-test/ps4-pm4-test/main.c via a path relative to CARGO_MANIFEST_DIR.
2. Parse the shared '#define IT_* 0x..' opcodes (IT_CLEAR_STATE/CONTEXT_CONTROL/DRAW_INDEX_AUTO/SET_CONTEXT_REG/SET_SH_REG) and assert each equals op::IT_*.
3. Also assert the CONTEXT/SH base windows referenced by the C comment (0xA000/0x2C00) match reg_base::CONTEXT/SH.
4. Document opcodes.rs as single source of truth in a comment.
5. Sanity-check the guard FAILS on a deliberate divergence, then revert.
6. Verify: cargo build --release, cargo test, clippy -D warnings, fmt --check, run_examples.sh check 6/6. Stay only in opcodes.rs; do NOT touch decode.rs/exec.rs/libscegnmdriver.
<!-- SECTION:PLAN:END -->

## Implementation Notes

Session 2026-07-11. Chose the Rust-test approach (no ELF rebuild / no OpenOrbis toolchain, fully headless) as preferred.

Done:
- Added `corpus_mirror_matches_opcodes` `#[cfg(test)]` in crates/gnm/src/pm4/opcodes.rs. At test time it reads examples/ps4-pm4-test/ps4-pm4-test/main.c via a path relative to CARGO_MANIFEST_DIR (../../examples/...), parses its `#define IT_* 0x..` lines, and asserts each of the 5 shared opcodes (IT_CLEAR_STATE/CONTEXT_CONTROL/DRAW_INDEX_AUTO/SET_CONTEXT_REG/SET_SH_REG) equals op::IT_*. Also asserts the corpus comment wording "CONTEXT base 0xA000"/"SH base 0x2C00" matches reg_base::CONTEXT/SH (the corpus references the bases in a comment, not a #define). Reg *offsets* (CONTEXT_CB_COLOR0_CLEAR_WORD0 etc.) are corpus-only, not defined in opcodes.rs, so intentionally not guarded.
- Documented opcodes.rs as the single source of truth in the module doc-comment.
- Sanity-checked the guard: temporarily set op::IT_DRAW_INDEX_AUTO 0x2D->0x2E; corpus_mirror_matches_opcodes FAILED with "PM4 opcode drift: corpus #define IT_DRAW_INDEX_AUTO = 0x2D but opcodes.rs (source of truth) op::IT_DRAW_INDEX_AUTO = 0x2E". Reverted to 0x2D.
- Skipped the optional D4 NID-derivation (mod.rs) to avoid conflicts with task-59/61.

Verify (all green): cargo build --release OK; cargo test all suites pass (0 failed); cargo clippy --all-targets --all-features -D warnings 0 errors (9 warnings are pre-existing ps4-syscalls SDK build-script notices, not lints); cargo fmt --check clean; ./scripts/run_examples.sh check = all 6/6 match baselines.

Files changed: crates/gnm/src/pm4/opcodes.rs only. main.c read-only.
Left uncommitted for maintainer.
