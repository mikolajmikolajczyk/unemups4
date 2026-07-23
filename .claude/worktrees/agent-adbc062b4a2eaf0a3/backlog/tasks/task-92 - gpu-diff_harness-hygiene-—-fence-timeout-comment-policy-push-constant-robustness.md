---
id: TASK-92
title: >-
  gpu: diff_harness hygiene — fence timeout, comment policy, push-constant
  robustness
status: Done
assignee: []
created_date: '2026-07-12 14:37'
updated_date: '2026-07-12 15:00'
labels:
  - gpu
  - gcn
dependencies: []
priority: low
ordinal: 91000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Round-9 code-review polish on the task-41 differential harness (test/dev binary, not shipped path). Three low-severity items: (2) wait_for_fences uses u64::MAX so a pathological recompiled shader hangs the maintainer binary forever with no progress — use a finite timeout and report GPU-timeout then continue the corpus; (3) two comments in companion_spirv.rs narrate WHAT the code does (Store into gl_Position / Forward each Location push-constant vec4) violating conventions.md comment policy — drop or reword to WHY; (4) render_vs hardcodes num_records at offset0 set0-bind0 VERTEX-stage; correct today NR_OFFSET=0 sole field but silently sends wrong bytes if the recompiler PC contract evolves — push per IoLayout.push_constants offset/size instead of the hardcoded 4-byte write, or assert the layout matches. Not blocking. task-91 (VS Y-flip readback) tracked separately.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 wait_for_fences uses a finite timeout; GPU hang reported as skip/timeout not an infinite block
- [x] #2 companion_spirv.rs narrating comments removed or reworded per conventions.md
- [x] #3 render_vs push-constant write is driven by IoLayout.push_constants (offset+size), not a hardcoded offset-0 4-byte write
- [x] #4 gate green: cargo test gcn, clippy -D warnings, fmt, oracle 6/6
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (round-9 hygiene, uncommitted — awaiting owner commit ok). 3 fixes in the task-41 diff harness (test/dev binary): (2) crates/gpu/src/bin/diff_harness.rs wait_for_fences u64::MAX -> GPU_FENCE_TIMEOUT_NS (5s); a hung recompiled shader now returns Err(TIMEOUT) -> [FAIL] per-shader + continue corpus, no infinite wedge. (3) companion_spirv.rs dropped 2 narrating comments (Store into gl_Position / Forward each Location push-constant vec4) per conventions.md comment policy. (4) render_vs push-constant now built from IoLayout.push_constants (write each field at its own offset_bytes by PushConstantRole) instead of a hardcoded offset-0 4-byte num_records write; correct today (NR_OFFSET=0 sole field) but now robust if the recompiler PC contract evolves. Gate: cargo build diff_harness ok, ps4-gcn 42 pass, clippy -D warnings 0 (gpu+gcn), fmt clean, run_examples 6/6. task-91 (VS Y-flip readback) stays separate. NOT committed.
<!-- SECTION:NOTES:END -->
