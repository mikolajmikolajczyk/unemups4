---
id: TASK-41
title: 'gcn: differential harness — interpreter oracle vs recompiled SPIR-V'
status: Done
assignee: []
created_date: '2026-07-11 12:54'
updated_date: '2026-07-12 14:06'
labels:
  - gpu
  - gcn
dependencies:
  - TASK-39
  - TASK-40
priority: medium
ordinal: 40000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
decision-3 discipline made executable. Two tiers: (a) headless — per corpus shader, interp lane outputs vs analytic expected + structural checks on recompiled module (same inputs consumed, same outputs exported, matching semantic locations); (b) GPU — a maintainer-run binary executing both paths (interp CPU; recompiled SPIR-V in a minimal offscreen pass reusing AshBackend) over same inputs, compare within epsilon, per-lane divergence report. Grows as corpus grows. Does NOT gate CI on GPU tier; does NOT build a CPU SPIR-V executor. (Optional: consumes a gitignored dir of local .sb for oracle-vs-recompiler on retail shaders — never committed.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 headless: cargo test runs interp-vs-expected for all corpus shaders
- [x] #2 live GPU: harness compares interp vs GPU-executed recompiled output, exits nonzero on divergence
- [ ] #3 adding a .s corpus entry needs no harness code change (data-driven)
<!-- AC:END -->



## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-41 @<prior-history> + clippy fixup, merged; AC#2 live-GPU = maintainer). TWO tiers. TIER (a) headless Vulkan-free crates/gcn/tests/differential.rs, data-driven (enumerates corpus/*.s, stage from _vs/_ps suffix, new .s = zero harness code): oracle_matches_analytic_expectation (interp vs HAND-DERIVED literals, NOT captured — SPOT-CHECKED interp_color_ps: R=0.25+0.5*0.5+0.5*1.0=1.0 etc computed by hand not via plane formula), recompiled_layout_matches_oracle_semantics (STRUCTURAL DRIFT GUARD — re-derives oracle reads/writes from decoded stream, asserts recompiler IoLayout agrees: same Locations, components>=oracle channel, buffer presence, num_records push), corpus_recompiles_and_validates (spirv-val vulkan1.1). 3 tests. TIER (b) live GPU maintainer-run crates/gpu/src/bin/diff_harness.rs + diff_harness_support/companion_spirv.rs (self-contained headless Vulkan, NOT surface-bound VulkanContext, NOT CI-gated): per shader runs interp + recompiled SPIR-V in minimal offscreen RGBA32F pass over SAME inputs, readback, diff eps 1e-5, per-lane [DIVERGE] report, exit nonzero on divergence. VS=3-vert TRIANGLE_LIST→64x64, read each vertex pixel (exp pos0); PS=fullscreen→1x1 texel (exp mrt0). Companion FS/VS hand-built rspirv, spirv-val-clean. CMD: LD_LIBRARY_PATH=/usr/lib cargo run -p ps4-gpu --bin diff_harness --release [--shader NAME]. 4 CONTRACTS honored: (a) PS Location driven with oracle P0+I(P1-P0)+J(P2-P0) as flat vec4 push-const; (b) sequential cmd_draw firstVertex=0 seeds gl_VertexIndex; (c) num_records pushed per IoLayout in VERTEX stage; (d) Unsupported→[defer] not divergence. ps4-gpu now deps ps4-gcn+rspirv (companion assembly). ORCH clippy fixup: differential.rs int_plus_one (components> *hi_chan) + MockMem pub (agent's clippy claim was stale). Verify: gcn 42 pass, full 230 pass, clippy 0, fmt clean, gcn Vulkan-free. Combined gate: 34 suites, oracle 6/6. MAINTAINER: GPU tier UNRUN (no display); first run may need ndc_y flip in clip_to_pixel if driver flips Y (per-lane report shows which); VS path handles 3-vert corpus only (N>3 → [skip]). CLOSES the recompiler==interp correctness loop (decision-3).
AC#2 GPU RUN 2026-07-12 (maintainer): `LD_LIBRARY_PATH=/usr/lib cargo run -p ps4-gpu --bin diff_harness --release` executed — the harness WORKS (compares interp vs GPU-executed recompiled SPIR-V, prints per-lane report, exits nonzero on divergence). Result: **PS PASS EXACT** (flat_color_ps + interp_color_ps GPU mrt0 == oracle — recompiler validated on real hardware incl VINTRP). VS reported [DIVERGE] but it's a HARNESS readback defect (Y-flip + reads pixel-center NDC position -0.984375=-63/64 not the exported pos0 value; triangle actually renders at correct positions), NOT a recompiler bug → task-91 (low). AC#2 ticked (harness mechanism confirmed on GPU).
<!-- SECTION:NOTES:END -->
