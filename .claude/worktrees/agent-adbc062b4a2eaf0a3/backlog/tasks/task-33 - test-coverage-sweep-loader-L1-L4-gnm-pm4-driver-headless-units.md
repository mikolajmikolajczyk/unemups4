---
id: TASK-33
title: 'test-coverage sweep: loader (L1-L4) + gnm pm4/driver headless units'
status: Done
assignee: []
created_date: '2026-07-11 05:29'
updated_date: '2026-07-11 05:43'
labels: []
dependencies: []
ordinal: 32000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fill headless test-coverage gaps in the loader (container/image/dynamic/nid/manager/linker) and gnm (pm4 opcodes/decode/trace, driver) subsystems that landed in task-26..31. Tests only — no production behavior change. gpu.rs and gpu backend.rs are intentionally out of scope (no logic / needs live Vulkan).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 manager.rs and pm4/opcodes.rs (currently zero direct tests) gain headless unit tests
- [x] #2 Genuine edge/error gaps filled across container/image/dynamic/nid/linker without duplicating existing tests
- [x] #3 cargo test --workspace green; cargo clippy -D warnings clean; cargo fmt no drift
- [x] #4 Oracle run_examples.sh check unchanged (tests don't affect runtime); no new deps; no non-test code changed except minimal pub(crate)/cfg(test) enablers
<!-- AC:END -->

## Notes
<!-- SECTION:NOTES:BEGIN -->
Test-only sweep, no production behavior changed. New unit tests by module: manager.rs +6 (allocator align/non-overlap/size-0, handle increment, register/get_by_name, resolve_symbol hit/miss/absolute, hle_export grouping — was ZERO tests); pm4/opcodes.rs +5 (name known/unknown, set_reg_base windows/None, reg_base constants — was ZERO tests); container.rs +3 (zero-segment malformed, truncated segment table, out-of-range phdr id); image.rs +3 (segment file-range beyond buffer error, PT_TLS extraction, no-TLS None); dynamic.rs +2 (oversized SCE table bounds no-panic, map_goblin_to_kind known-arms + Unknown fallback); nid.rs +1 (exact multi-char encode_id values); linker.rs +8 (Relative, TpOff64, Absolute64 local symbol_value, lazy-stub emission for unresolved import + stub bytes, unresolved-without-init error, GlobDat resolve, load_executable entry).

Workspace tests: 63 -> 92 passed (+29), 0 failed, 3 ignored (unchanged; the copyrighted-dump oracles). clippy -D warnings clean, cargo fmt clean.

Minimal test-only enabler: linker.rs MockMemory now bump-places `map(0,..)` ("map anywhere", used by init_stubs) at a high non-overlapping base instead of literally addr 0, so the lazy-stub path is exercisable. Existing tests map at explicit non-zero bases so are unaffected. No pub/pub(crate) visibility changes needed.

REAL bugs found: NONE.

Deliberately uncovered: crates/core/src/gpu.rs (trait + plain data types, no logic to test) and crates/gpu/src/backend.rs (AshBackend needs a live Vulkan device — not headless-unit-testable; refused to fabricate a fake device/mock backend just to hit a number).

Oracle run_examples.sh check: the ONLY diffs across all six examples are the two pre-existing known artifacts (headless "Failed to initialize Vulkan" line + task-20 "Loaded libSceGnmDriver.so" line); grep confirmed zero other +/- lines. Test-only changes do not touch runtime.

NB: filed as TASK-33 (the CLI auto-assigned the next id; the prompt referred to it as task-32).
<!-- SECTION:NOTES:END -->
