---
id: TASK-202
title: >-
  gnm/gpu: lamp lights render as flat yellow instead of the console's soft
  blurred glow
status: To Do
assignee: []
created_date: '2026-07-21 17:21'
labels:
  - gnm
  - gpu
  - celeste
  - retail
  - observation
dependencies: []
priority: medium
ordinal: 207000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
OBSERVATION ONLY (maintainer, 2026-07-21) — not scheduled, recorded so it is not lost. After task-199 (multi-texture PS binding) and task-201 (RT-as-texture honours the guest S#) the in-game scene renders correctly and pixel art is crisp, but the lamp posts differ from hardware: on the real PS4 each lamp casts a soft, blurred pool of light; on ours the lamp is a FLAT YELLOW patch with no falloff.\n\nWhat this most likely touches (not yet investigated, do not assume): the glow/bloom pass that spreads the light sprite, an additive blend that is being applied as a replace, or a radial-gradient light texture that is sampled without its intended filtering/blend. Note task-201 established that four draws in this frame genuinely ask for LINEAR filtering (the bloom chain) while the rest ask NEAREST — so a glow that lost its blur is plausibly a blend/pass problem rather than a filter one, but that is a hypothesis, not a finding.\n\nWe are unusually well-equipped to answer this quickly: dumps/scrape2 already holds a real-PS4 capture of THIS EXACT scene (lamps included), and tools/ps4-gnm-scrape/host/src/bin/framediff.rs diffs a console frame against one of our gpu-snapshot frames per draw — registers, blend, descriptors, S# requested vs sampler actually bound. Start there rather than reasoning from the emulator side; that method is what cracked the yellow sky and the blur.\n\nOracle: lamps show a soft blurred light with falloff matching the console — maintainer live PNG oracle. Provenance: AMD GCN ISA / Mesa / llvm-mc only; never another PS4 emulator.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the console-vs-ours difference for the lamp/glow draws is identified with framediff (which draw, which state) before any code change
- [ ] #2 lamps render with a soft blurred glow matching hardware — maintainer live oracle
<!-- AC:END -->
