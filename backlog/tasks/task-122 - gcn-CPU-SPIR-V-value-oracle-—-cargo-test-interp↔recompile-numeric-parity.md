---
id: TASK-122
title: 'gcn: CPU SPIR-V value oracle — cargo-test interp↔recompile numeric parity'
status: Done
assignee: []
created_date: '2026-07-16 06:15'
updated_date: '2026-07-16 07:53'
labels:
  - from-code-review
  - gcn
  - test
dependencies: []
ordinal: 128000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding (differential.rs:27-31): the differential test deliberately does NOT build a CPU SPIR-V executor — it only asserts IoLayout equality. So every per-op 'interp matches recompile bit-for-bit' claim ships UNVERIFIED by cargo test; value-level agreement is checked only by the manually-run diff_harness on a real GPU. Any op divergence (wrong operand order, rounding, swapped src) passes CI green. Fix: run a CPU SPIR-V interpreter over the recompiled module in-test and compare to interp per lane over the corpus (within a documented ULP/epsilon for transcendentals).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 cargo test (no GPU) executes each corpus shader's recompiled SPIR-V on CPU and compares per-lane vs interp oracle
- [x] #2 a deliberately-injected op mismatch (e.g. swap a*b operand order in emit) makes the test RED
- [x] #3 transcendental-class ops (sin) compared within a documented ULP budget, not exact
<!-- AC:END -->

## Implementation Notes
<!-- SECTION:NOTES:BEGIN -->
Added a GPU-free CPU SPIR-V value evaluator that, in `cargo test`, re-executes each
corpus shader's recompiled SPIR-V per-live-lane and compares its exports to the interp
oracle per lane/channel — closing the dual-oracle gap (previously only IoLayout equality
+ spirv-val ran in CI; value parity was checked only by the maintainer-run GPU diff_harness).

Approach: parse the assembled words back with `rspirv::dr::load_words` and walk the single
straight-line basic block the recompiler emits (no CFG/loops/phi), keeping an id→Value SSA
map + a var→Value memory map (GCN regs are function-local OpVariable pairs). Interface I/O
is recognized by decorations. NO new dependency — reused `rspirv` 0.13 (already used to
BUILD the SPIR-V) and `half` 2 (already used by interp+recompile for f16 pack/unpack),
deliberately avoiding a full CPU SPIR-V executor crate (license/dep churn for a small closed
op subset). GPL-3.0 compatible: no new deps to vet.

Files: `crates/gcn/tests/spirv_eval/mod.rs` (new evaluator, in a subdir so Rust does not
compile it as its own test binary; pulled into differential.rs via `mod spirv_eval;`) and
`crates/gcn/tests/differential.rs` (new `recompiled_spirv_matches_oracle` test + per-lane
binding reconstruction from the SAME `build_launch` that drives the oracle). recompile.rs
and interp.rs unchanged.

Comparison: bit-for-bit (`to_bits()`) for every corpus export EXCEPT the sole sin shader
(`vop3_mad_sin_fract_ps`), which uses a documented 1e-6 absolute budget (AC #3) because host
`sinf` is ULP-class, not correctly rounded. fract/pkrtz boundary cases are exact for the
corpus inputs and noted at the compare site.

AC #2 demo: flipped `V_CMP_LT_F32` emit from `f_ord_less_than` → `f_ord_greater_than` in
recompile.rs::emit_vopc. `recompiled_spirv_matches_oracle` went RED — `cmp_cndmask_ps: lane 0
Mrt(0) ch0 — CPU-SPIR-V 0.25 != oracle 0.75 (bit-for-bit)` — while `corpus_recompiles_and_validates`
(spirv-val) and `recompiled_layout_matches_oracle_semantics` (structural) stayed GREEN, proving
the VALUE oracle (not spirv-val) catches the op divergence. Injection reverted; tree clean.

Verify: `cargo test -p ps4-gcn` 64 passed / 2 ignored (differential now 4 tests incl. the new
one over all 21 spec'd corpus shaders); `cargo clippy -p ps4-gcn --all-targets` clean;
`cargo fmt --check` clean.
<!-- SECTION:NOTES:END -->
