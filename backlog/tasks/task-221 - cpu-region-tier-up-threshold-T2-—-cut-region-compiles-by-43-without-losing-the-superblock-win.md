---
id: TASK-221
title: >-
  cpu: region tier-up threshold T2 — cut region compiles by 43% without losing
  the superblock win
status: Done
assignee: []
created_date: '2026-07-22 09:56'
updated_date: '2026-07-22 10:05'
labels:
  - cpu
  - perf
  - jit
dependencies:
  - TASK-219
priority: medium
ordinal: 226000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-219 enabled superblocks and measured the bill: compile_ns went from about 17 s to 36.3 s for 5931 regions on a Celeste gameplay run, for a 5-8% guest_exec improvement.

The reason it costs so much is that we left x86jit's T2 at its default, which is T1. Vm::set_tier_up_region_after is documented as a HIGHER bar than the block threshold, so short loops never pay a wasted region compile — but tier_up_region_after.unwrap_or(thr) means an unset T2 is simply T1, and ours was unset. A loop earned a full region compile after the same 50 executions as an ordinary block.

Derived from our own numbers: 36.3 - 17.0 s over 5931 regions is about 3.25 ms per region compile, against 17.0 s over ~130000 misses, about 0.131 ms per block. A region costs roughly 25x a block.

Set T2 to 10x T1 (500), with UNEMUPS4_REGION_TIER_UP to override without a rebuild (0 pins T2 back to T1, which is what task-219 shipped and what the A/B compares against).

Measured at comparable miss counts:

    T2 = T1 (50)   regions 3624   compile_ns 19.61 s
    T2 = 500       regions 2054   compile_ns 13.19 s

43% fewer regions, 33% less compile time.

WHAT IS NOT YET MEASURED, and it is the point of the change: whether guest_exec holds at the task-219 level with the cheaper threshold. The attract scene this was measured on does not reach the guest-bound regime reliably — only the maintainer's gameplay does, where the baseline is guest_exec ~22-23 ms at flip ~12 ms. If guest_exec regresses, 500 is too high and the value should come down; the arithmetic above justifies a higher-than-T1 threshold, not this particular number.

Also unresolved and worth watching: whether fewer regions means the hot loops still get regions and only the cold ones were dropped, or whether genuinely hot code is now missing out. The region count alone cannot tell those apart.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 T2 is set above T1 with a rationale derived from measured region-vs-block compile cost, overridable by UNEMUPS4_REGION_TIER_UP
- [x] #2 measured region count and compile_ns before/after at comparable miss counts
- [x] #3 measured on the maintainer's gameplay that guest_exec holds at the task-219 level, or the threshold is lowered until it does
- [x] #4 build + clippy clean, cargo test --workspace green
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed and CONFIRMED on maintainer gameplay 2026-07-22.

T2 = 10x T1 (500), overridable by UNEMUPS4_REGION_TIER_UP (0 pins it back to T1, i.e. task-219's behaviour).

RATIONALE, derived rather than guessed: enabling regions moved compile_ns ~17 s -> 36.3 s for 5931 regions, about 3.25 ms per region compile, against ~0.131 ms per block over ~130000 misses. A region costs roughly 25x a block, so a threshold treating the two alike cannot be right. x86jit resolves an unset T2 as tier_up_region_after.unwrap_or(thr), so ours was silently equal to T1 despite its own docs calling T2 a higher bar.

MEASURED on gameplay, guest_exec at matched flip ~12 ms across the whole progression:

    baseline (opt none, no regions)   23.93 / 24.06
    superblocks, T2 = T1 (50)         21.92 / 22.04 / 22.89
    superblocks, T2 = 500             20.88 / 20.90 / 20.92 / 21.00 / 21.20 / 21.23

About 13% off guest_exec against the starting point, and the spread tightened as well — 20.88-21.24 where T2=T1 ran 21.92-22.89. Sustained 28.8-29.8 fps across many consecutive windows; the maintainer reports the first gameplay above 30 fps and the smoothest session so far.

COST fell at the same time: compile_ns 36.3 s -> 30.5 s (-16%) and regions 5931 -> 4046 (-32%) at comparable miss counts.

That settles the open question in the description. Fewer regions meant the COLD loops were dropped, not that hot code missed out — had genuinely hot loops lost their regions, guest_exec would have risen rather than fallen. AC #3 ticked on this evidence.

Not claimed: 500 is not shown to be optimal, only better than 50 on this title. The env override exists so the next title can be checked without a rebuild.
<!-- SECTION:NOTES:END -->
