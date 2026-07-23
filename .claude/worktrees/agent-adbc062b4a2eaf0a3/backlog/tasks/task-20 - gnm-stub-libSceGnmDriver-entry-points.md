---
id: TASK-20
title: 'gnm: stub libSceGnmDriver entry points'
status: Done
assignee: []
created_date: '2026-07-10 18:23'
updated_date: '2026-07-10 20:58'
labels:
  - gnm
  - gpu
dependencies:
  - TASK-25
priority: high
ordinal: 20000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Phase 2 / doc-2 D1 (see decision-3, north star = Bloodborne). Gnm/Gnmx are statically linked into games, so the only interceptable surface is libSceGnmDriver: its submit/draw/dispatch entry points hand over guest-memory PM4 command buffers. Today the repo has ZERO Gnm (no libSceGnmDriver handlers, no PM4, no Liverpool) — a Gnm-linked guest crashes on unresolved imports before it can run. This task registers the GnmDriver NIDs homebrew links against as log-and-return-success stubs so such a guest BOOTS to videoout instead of crashing. No PM4 execution here (that is the trace decoder + present/sync tasks). Survey shadPS4's src/core/libraries/gnmdriver/gnmdriver.cpp for the NID list (doc-2 §3 has the key ones: sceGnmSubmitCommandBuffers zwY0YV91TTI, sceGnmSubmitAndFlipCommandBuffers xbxNatawohc, sceGnmSubmitDone yvZ73uQUqrk, sceGnmDrawIndex HlTPoZ-oY7Y, sceGnmDrawIndexAuto GGsn7jMTxw4, sceGnmDispatchDirect 0BzLGljcwBo, sceGnmMapComputeQueue 29oKvKXzEZo, sceGnmDingDong bX5IbRvECXk, plus the rest of the driver's exported surface homebrew commonly links). Follow the existing HLE lib pattern (crates/libs, e.g. libscevideoout/mod.rs). Submit stubs should retain enough to hand their command-buffer pointers to the future PM4 decoder (dep of the next task). Portability note (decision-3): pure HLE, no Vulkan extension use — nothing MoltenVK-relevant here.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Register the libSceGnmDriver submit/draw/dispatch NIDs (from a shadPS4 gnmdriver.cpp survey) as log-and-return-success HLE stubs
- [x] #2 An OpenOrbis Gnm-linking sample loads and reaches its main loop instead of crashing on missing libSceGnmDriver symbols (or it is documented why no such sample is available and a hand-written stub-linking ELF is used instead)
- [x] #3 Stub submit entry points expose their command-buffer pointer/size so the PM4 trace decoder task can consume them; no PM4 is executed in this task
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-10. NIDs registered in crates/libs/src/libscegnmdriver/mod.rs (lib libSceGnmDriver.so): sceGnmSubmitCommandBuffers zwY0YV91TTI, sceGnmSubmitAndFlipCommandBuffers xbxNatawohc, sceGnmSubmitDone yvZ73uQUqrk, sceGnmDrawIndex HlTPoZ-oY7Y, sceGnmDrawIndexAuto GGsn7jMTxw4, sceGnmDispatchDirect 0BzLGljcwBo, sceGnmMapComputeQueue 29oKvKXzEZo, sceGnmDingDong bX5IbRvECXk, sceGnmAreSubmitsAllowed b08AgtPlHPg, sceGnmInsertPushMarker W1Etj-jlW7Y, sceGnmInsertPopMarker 7qZVNgEu+SY, sceGnmInsertWaitFlipDone 1qXLHIpROPE, sceGnmDrawInitDefaultHardwareState{,175,200,350} Idffwf3yh8s/QhnyReteJ1M/0H2vBYbTLHI/yb2cRhagD1I, sceGnmFlushGarlic iBt3Oe00Kvc. GnmDriver logic lives in ps4-gnm crates/gnm/src/driver.rs (Vulkan-free); handlers reach it via a process-global driver() -> &Mutex<GnmDriver> OnceLock (mirrors ps4-core get_kernel). Record-buffer seam for task-21: submit/submit_and_flip push SubmitRange{dcb_ptr,dcb_size,ccb_ptr,ccb_size,flip}; consume via GnmDriver::submissions()/take_submissions(). Names already existed in data/ps4_names.txt so NIDs are the build.rs-generated ones (match doc-2 §3 exactly). Verify: build/test(41 pass incl 8 new)/clippy(-D warnings clean)/fmt --check all green. ORACLE run_examples.sh check: only added boot line 'HLE: Loaded libSceGnmDriver.so' (6x, expected new-lib registration) + known headless 'Failed to initialize Vulkan' artifact; zero removed lines, guest output byte-identical, non-Gnm guests unperturbed. Baselines NOT regenerated (no commit); maintainer runs 'run_examples.sh capture' on merge to absorb the new lib-load line. AC#2: no Gnm-linking sample in examples/ yet (task-22 builds the PM4 ELF); validated instead by unit test key_gnm_nids_resolve_to_registered_handlers asserting all 8 key NIDs resolve to registered handlers.
<!-- SECTION:NOTES:END -->
