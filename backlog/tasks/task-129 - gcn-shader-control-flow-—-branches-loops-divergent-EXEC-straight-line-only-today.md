---
id: TASK-129
title: >-
  gcn: shader control flow — branches, loops, divergent EXEC (straight-line-only
  today)
status: To Do
assignee: []
created_date: '2026-07-16 06:25'
updated_date: '2026-07-16 10:36'
labels:
  - from-audit
  - gcn
dependencies: []
ordinal: 135000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Hardcode audit (game#2 risk, Tier-2 DEFERS ENTIRE SHADER — the single biggest wall for a second game): the recompiler is straight-line-only (recompile.rs:17 'the corpus has no branches'), emitting one basic block. All control-flow opcodes (s_branch, s_cbranch_scc0/vccz/execz/execnz, loop pseudos) hit the catchall and return UnsupportedInst; EXEC saves are discarded (recompile.rs:989) and predication collapses to a single per-invocation bool (lane-0), so divergent execution is not modeled. Celeste's 22 shaders are branchless; almost any other game's shaders branch for quality tiers, alpha-test, lighting, loops. Result: the whole shader defers → that game renders nothing. Fix: build a real CFG in the recompiler (structured control flow / SPIR-V selection+loop merges) and model divergent EXEC as a per-invocation predicate over each block. Large; likely multi-step. Pairs with the differential/interp discipline (interp must model the same branching).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 recompiler builds a CFG and lowers s_branch + s_cbranch_* to SPIR-V structured control flow
- [ ] #2 divergent EXEC modeled per-invocation; interp oracle mirrors the same control flow
- [ ] #3 a branching corpus shader recompiles + passes differential/decode goldens
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Design pass 2026-07-16 (Plan agent). DECISION: structured SPIR-V CF (OpSelectionMerge/OpLoopMerge/OpBranchConditional), NOT EXEC-mask emulation — SPIRV-Cross→Metal REQUIRES structured merge blocks (portability floor decision-3); per-invocation model (doc-6 Entry 8) makes a wave branch degenerate to a per-lane scalar branch, driver handles reconvergence.

SHARED CFG FIRST (new crates/gcn/src/cfg.rs, backend-neutral): Vec<BasicBlock>=slice of Decoded + Terminator{fallthrough|branch|cond{cond,taken,fall}|return}. Leaders=entry+branch targets+post-branch. Branch target = offset_dwords+size_dwords+sign_extend_i16(simm16) (dwords). Decoder ALREADY emits Inst::Sopp{op,simm16} + opcodes::sopp S_BRANCH/S_CBRANCH_* — nothing missing at decode.

INTERP (wave): replace linear walk (interp.rs:295-315) with CFG walk keyed on exec; branch preds are whole-wave (vccz=vcc&exec==0, execz=exec==0, scc). Divergent EXEC via structured mask stack: split exec at forward branch, run taken then fall under narrowed exec, OR-restore at merge (post-dominator). scc production (s_cmp/carry) needed for scc-branches — start with vccz/execz (no scc).

RECOMPILE (per-invocation): block-builder replaces single begin_block/ret (recompile.rs:587/670). Forward cond → OpSelectionMerge+OpBranchConditional using Entry-8 predicate-bool. KEEP load/store register model (Function OpVariable) across blocks = correct last-writer-wins with NO hand-rolled phi (glslang does this; spirv-val accepts). BUT back the predicates map by a Function bool OpVariable (pred_vars) too — cached SSA value ids aren't valid cross-block. execz/execnz degenerate per-invocation (running lane always live) — handle explicitly.

CPU SPIR-V ORACLE (task-122, tests/spirv_eval/mod.rs): must go multi-block or it stops guarding. Block map + fetch-execute loop; add OpBranch/OpBranchConditional/(Selection|Loop)Merge-noop/Return terminators. No OpPhi needed if load/store kept (vars map survives jumps). Add block-visit cap (no hang).

SEQUENCE: (1) cfg.rs +unit tests. (2) spirv_eval multi-block early. (3) recompile forward-only s_cbranch + interp single-level EXEC split/merge + corpus cbranch_alpha_ps → green differential = AC milestone. (4) if-else diamond corpus. (5) task-131 μop extraction (mechanical, differential-guarded, AFTER branching green; control flow stays OUT of μop layer — μops=straight-line dataflow, branching=CFG terminator). (6) loops (OpLoopMerge, back-edge, scc) last.

MINIMAL FIRST SLICE: forward-only s_cbranch_vccz single if (no else/loop), 1 new PS corpus shader, load/store regs (no phi), one EXEC narrow+restore.

RISKS: (1) real retail CF is reducible-but-arbitrary → needs a relooper-class structurizer; scope 129 to structured/reducible, defer irreducible to follow-up, fail clean to UnsupportedInst never unstructured SPIR-V. (2) confirm Function-var-read-after-merge passes spirv-val 1.1 + MoltenVK; fallback = OpPhi at merge only. (3) interp reconvergence must match HW structured reconverge — differential is the guard, pin each rule with a corpus shader. (4) execz skip-blocks (retail sh02 next wall) degenerate in recompile but real divergence in interp — test dropped-lane→no-export path. Full design in agent transcript.

PROGRESS 2026-07-16 (slice 4 — if-ELSE DIAMOND, done): extended forward-only single-`if` to a two-arm diamond. cfg.rs: added `Cfg::merge_target(bi)` (post-dominator of a `Cond` block via unconditional-successor reach from both arms; handles single-`if` degenerately and the diamond) + `if_else_diamond_splits_four_blocks_and_finds_merge` unit test. recompile.rs: `emit_cfg` now names the merge from `merge_target` (was hardcoded `taken`), emits OpSelectionMerge merge + OpBranchConditional to the two arm blocks, each arm OpBranch→merge; verified structured (spirv-dis) with both arms storing the SAME Function VGPR vars → last-writer-wins per lane, NO OpPhi. interp.rs: rewrote `execute_cfg` to run BOTH arms EXEC-narrowed (taken-mask then fall-mask) via new `run_region`/`run_block_body` helpers, OR-restore at the merge; nested Cond/return inside an arm defers clean. New corpus shader `cbranch_select_ps` (self-authored, PS_J): v_cmp_lt→vccz to dark arm, bright arm s_branch's over it, both merge→exp; differential + task-122 value-oracle both green, spirv-val vulkan1.1 PASS. All existing corpus (incl. single-`if` cbranch_alpha_ps now through the generalized path) still green. NEXT per plan: (5) task-131 μop extraction, then (6) loops (OpLoopMerge/back-edge/scc).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
STRUCTURED CONTROL FLOW COMPLETE (merged @main): slice-1 forward single-if, slice-4 if-else diamond, loops slice (reducible natural loops, OpLoopMerge, no-phi, interp iteration cap 1<<16, spirv_eval visit cap 1024). All via shared cfg.rs (CFG builder + merge_target post-dominator + reducible-loop validate). Decision: structured SPIR-V CF (OpSelectionMerge/OpLoopMerge/OpBranchConditional) NOT EXEC-mask emulation — MoltenVK/SPIRV-Cross requires structured merge; per-invocation model degenerates wave branch to per-lane scalar. Load/store Function-var registers = NO phi. corpus: cbranch_alpha_ps, cbranch_select_ps, loop_accum_ps. differential + task-122 CPU value oracle + spirv-val vulkan1.1 GREEN over all. REMAINING: task-131 μop semantics layer (extract per-op semantics written-once, mechanical differential-guarded refactor); irreducible/multi-exit CF still defers clean (relooper-class structurizer = follow-up if a real shader needs it).
<!-- SECTION:NOTES:END -->
