---
id: TASK-173
title: 'gpu/gnm: honor S# sampler wrap field (CLAMP_X/Y) instead of hardcoding REPEAT'
status: Done
assignee: []
created_date: '2026-07-18 14:23'
updated_date: '2026-07-18 14:41'
labels:
  - gpu
  - gnm
  - celeste
  - sampler
  - retail
dependencies: []
priority: medium
ordinal: 177000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-170 sampler check found decode_s_sharp (crates/gnm/src/vbuf.rs:426) reads ONLY the S# filter bit; all 3 SamplerDesc constructions in crates/gnm/src/exec.rs (:1048/:1149/:1235) hardcode SamplerAddressMode::Repeat. Real Celeste draw1 (1500x199 banner/backdrop) specifies CLAMP_EDGE (S# word0[2:0]=CLAMP_X, [5:3]=CLAMP_Y) but we render it REPEAT -> backdrop tiles where it should clamp; contributes to task-171 edge/bleed artifacts. NOT the snow snap (snow is WRAP on both sides). Fix: decode CLAMP_X=word0[2:0], CLAMP_Y=word0[5:3] (GCN codes 0->Repeat, 1/3->MirrorRepeat, 2/4/6->ClampToEdge), carry U/V modes through SamplerState + SamplerDesc, add MirrorRepeat to SamplerAddressMode, set address_mode from the decoded S# at the 3 exec.rs sites, extend vk_address_mode. Verify vs real S# wrap fields (real corpus: draw1=CLAMP, draw2 snow=WRAP, draw3 atlas=WRAP).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 decode_s_sharp reads CLAMP_X/Y and maps GCN clamp codes to address modes
- [x] #2 the 3 exec.rs SamplerDesc sites set address_mode from the decoded S#, not hardcoded Repeat
- [x] #3 real Celeste draw1 renders CLAMP_EDGE (backdrop no longer tiles); snow/atlas unchanged (WRAP); build+test+clippy clean
<!-- AC:END -->
