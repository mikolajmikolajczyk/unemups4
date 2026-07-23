---
id: TASK-219
title: >-
  cpu: form superblocks — one basic block per compiled unit starves both
  dead-flag elimination and Cranelift's mid-end
status: Done
assignee: []
created_date: '2026-07-22 09:36'
updated_date: '2026-07-22 09:46'
labels:
  - cpu
  - perf
  - jit
dependencies: []
priority: high
ordinal: 224000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Guest x86 execution is about 25 ms of a 40 ms Celeste gameplay frame, 99% on-core (task-215), so it is genuine computation and the single largest cost. Two x86jit improvements aimed at it changed nothing measurable on this title:

- Cranelift opt_level none -> Speed (x86jit task-276): guest_exec 23.9-25.3 ms before, 24.0-27.8 ms after. Confirmed active, not assumed — the emulator now logs the resolved codegen and prints opt_level=Speed host=Native superblocks=false verifier=false.
- IBTC miss-path probe (x86jit, -40% on its own indirect bench): no effect here, and the reason is quantified — chained transfers run about 1000000 per frame against roughly 5000 indirect fast hits, so indirect dispatch is ~0.5% of control transfers.

Both null results point the same way: the compiled unit is too small for any optimizer to work with. We construct JitBackend::new(), which passes caps: None, so no regions are ever formed and the counters show regions=0 throughout.

Why that gates everything, not just a little:

- x86jit deliberately does NOT model lazy flags (x86jit-core/src/state.rs:123 records this as a deferred Variant B) and substitutes compile-time dead-flag elimination (lift/mod.rs:767, described there as the compile-time form of lazy flags). It can only drop a flag computation it can see is never read. At one basic block per unit, flags must be materialized at nearly every boundary because a successor might read them — and we cross ~1M boundaries per frame.
- Cranelift's mid-end has the same problem from the other side: no loop inside the unit means nothing for LICM to hoist and little for GVN to merge, which is the likeliest reason opt_level=Speed measured as no change at all.

Enable region formation via JitBackend::with_superblocks. Use with_superblocks rather than with_options: the latter PINS opt_level, while we want the level x86jit derives from the tier-up policy.

Caps mirror x86jit-cli's production BG_REGION_CAPS (max_blocks 16, max_icount 256). Regions are formed only for loops (IrRegion::has_loop), so loop-free code stays single-block and pays no region compile.

Keep an escape hatch. UNEMUPS4_SUPERBLOCKS=0 restores single-block compilation. This session has already had one case where a retained escape hatch (the fixed-step clock) turned an unexplained collapse into a clean A/B instead of a guess.

Measure guest_exec per frame and frames-per-window on the same scene, plus compile_ns — regions are heavier to compile and x86jit gates them behind a higher threshold (set_tier_up_region_after, T2) for exactly that reason. Watch for the pattern this investigation keeps hitting: removing cost from one phase has repeatedly MOVED wall time rather than shortening the frame, so report the frame rate, not only guest_exec.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 superblock formation is enabled by default with production caps, with UNEMUPS4_SUPERBLOCKS=0 as an escape hatch
- [x] #2 the resolved codegen line reports superblocks=true, so the setting is confirmed rather than assumed
- [x] #3 measured before/after on the same scene: guest_exec per frame, frames-per-window and compile_ns, all reported even if guest_exec does not improve
- [x] #4 build + clippy clean, cargo test --workspace green; maintainer confirms the title still renders and plays correctly
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed 2026-07-22.

ENABLED: JitBackend::with_superblocks(REGION_CAPS) in crates/cpu/src/guest_vm.rs, caps mirroring x86jit-cli's production BG_REGION_CAPS (max_blocks 16, max_icount 256). with_superblocks rather than with_options, because the latter PINS opt_level while we want the level x86jit derives from the tier-up policy. Escape hatch UNEMUPS4_SUPERBLOCKS=0.

REGRESSION CAUGHT BY THE TEST SUITE, and the fix is not a workaround. The first attempt failed crates/cpu/tests/dirty_source.rs with 'loop must execute JIT-compiled (JIT hits > 0) or the race is not exercised'. That test uses new_eager_jit_for_test, which deliberately turns background tier-up OFF as a determinism lever. x86jit documents regions as meaningful only with a region-forming backend AND background tier-up — a region is a heavier compile that pays off across many loop iterations, which is exactly what the background worker and the higher T2 threshold arrange. So superblocks + eager-foreground is a self-contradictory configuration. Region formation now additionally requires TierUp::HotBackground.

MEASURED (maintainer gameplay, matched on flip so the scenes are comparable):

    flip ~12 ms   baseline 23.93 / 24.06     superblocks 21.92 / 22.04 / 22.89
    flip ~15 ms   baseline 25.18 / 23.97     superblocks 23.12 / 23.55 / 24.51

About 5-8% off guest_exec. Real, but 2 ms of a 35 ms frame — the maintainer reports no visible difference, which is consistent rather than contradictory.

COST: compile_ns 36.3 s against ~16-18 s in a comparable run without regions, so more than double, for that 5-8%. regions=5931, confirmed forming (was 0 throughout every earlier run). We did NOT set set_tier_up_region_after, so regions form at the same threshold (50) as ordinary blocks; x86jit provides that separate, higher T2 threshold precisely so short loops do not pay for a wasted region compile. Raising it is the obvious next lever and would likely keep most of the gain at a fraction of the compile cost.

WHAT THIS SETTLES about the wider question: three JIT-side levers — opt_level=Speed, the IBTC miss-path probe, and now superblocks — total a few percent between them, against a gap to console hardware of roughly 5-10x. That gap will not close with JIT knobs.

One alarming hypothesis checked and REJECTED while here: if Mono were running interpreted rather than full-AOT, the guest would execute many times more instructions and no JIT tuning could help. The slow-frame RIP samples are spread wide (top address 3.3%, dozens of contributors), whereas a Mono interpreter would concentrate them in a handful of dispatch-loop addresses. The guest is executing real AOT code.

Which leaves codegen quality or structural emulation cost, and task-220 is what distinguishes them: we still measure only milliseconds, never instructions, so we cannot say whether 25 ms is many instructions run slowly or an abnormal number of instructions.

Build + clippy clean on the crates touched, cargo test --workspace 575 green, maintainer played the title with no visual or behavioural change.
<!-- SECTION:NOTES:END -->
