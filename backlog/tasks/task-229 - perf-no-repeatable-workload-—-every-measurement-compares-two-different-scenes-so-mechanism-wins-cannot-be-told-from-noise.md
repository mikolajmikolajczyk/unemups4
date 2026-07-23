---
id: TASK-229
title: >-
  perf: no repeatable workload — every measurement compares two different
  scenes, so mechanism wins cannot be told from noise
status: To Do
assignee: []
created_date: '2026-07-22 14:37'
labels:
  - perf
  - diag
  - test
dependencies: []
priority: high
ordinal: 234000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Four consecutive x86jit changes were measured this session by playing Celeste and reading the profiler. Each time the mechanism moved exactly as designed and guest throughput did not, and each time the comparison was contaminated because the two runs were not the same scene.

The last one is the clearest case. x86jit 8a67575 vs 204dc0a (sinking the watched-store helper call out of the hot instruction stream):

                              8a67575          204dc0a
    IPC                       1.02             1.17      +15%
    stalled-cycles-frontend   51.1% of cycles  48.7%     -2.4 pp
    iTLB-load-misses          38.09 M          33.68 M   -12%
    guest MIPS                135-144          138-143   unchanged
    guest instructions/frame  2.5-3.0 M        3.8-4.6 M  <- DIFFERENT SCENE
    fps                       34-40            25-30      <- DIFFERENT SCENE

IPC improved 15%. It is not possible to say from this whether that is the change or the scene, because a heavier scene has a different instruction mix. The only scene-robust number available is MIPS over guest-execution time, and it is flat — but "flat MIPS" is a weak instrument when the frames either side of it differ by 60% in guest work.

WHAT IS MISSING: a deterministic workload. Same code path, same number of frames, no human at the controls, run twice under two pins and diffed. Without it every future perf claim about the CPU backend is unfalsifiable, and this project has already produced three true measurements with false conclusions on exactly this axis (task-220 blamed the lift, task-227 blamed the write barrier, and the region measurements bounded dispatch at 6-8% only because two configurations happened to be comparable).

SHAPE OPTIONS, cheapest first — this needs deciding before building:

1. Fixed-step clock + scripted input. `UNEMUPS4_CLOCK=fixed-step` already makes guest time a function of the flip count rather than wall time (decision-8), which is most of determinism. Add a recorded pad-input track played back from frame 0, run N frames, exit, print the profiler summary. Celeste's attract loop may even suffice with NO input, which would make this nearly free.

2. Headless replay of a captured submit stream. The GNM scrape (task-168) already captures real DCB/CCB. Replaying a fixed capture exercises the GPU path deterministically but not the guest CPU, so it answers a different question — useful, not this.

3. A synthetic guest ELF that runs a known instruction mix. Fully deterministic and fully unrepresentative of Mono AOT's flat 58k-block profile, which is the shape that actually matters here. Only useful as a floor.

Option 1 is the one that measures what we care about. Worth checking first whether the attract loop alone gives a stable enough MIPS figure across two runs of the same pin — if it does, that IS the benchmark and nothing needs building.

ACCEPTANCE SHAPE: two runs of the SAME build must agree closely enough that a real 5% change is visible above the spread. State the measured run-to-run spread; if it is worse than 5%, say so rather than shipping a benchmark that cannot resolve what it is for.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 a single command produces a fixed-length Celeste run with no human input and prints the profiler summary
- [ ] #2 run-to-run spread of guest MIPS across two runs of the same build is measured and stated
- [ ] #3 the spread is small enough to resolve a 5% change, or the task records that it is not and why
- [ ] #4 the 8a67575 vs 204dc0a comparison is redone under it, replacing this session's contaminated numbers
<!-- AC:END -->
