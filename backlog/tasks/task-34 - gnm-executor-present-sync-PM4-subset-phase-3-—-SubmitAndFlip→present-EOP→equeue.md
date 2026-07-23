---
id: TASK-34
title: >-
  gnm: executor present/sync PM4 subset (phase 3) — SubmitAndFlip→present,
  EOP→equeue
status: Done
assignee: []
created_date: '2026-07-11 08:43'
updated_date: '2026-07-11 13:42'
labels:
  - gnm
  - gpu
dependencies:
  - TASK-21
priority: high
ordinal: 33000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Phase 3 of the GPU roadmap (doc-2 §7 step 5, decision-4). The BRIDGE between decode/trace (task-21, done) and draw (task-24). With the PM4 decoder walking submitted command buffers (task-21) and the GnmDriver recording SubmitRanges (task-20), grow the ps4-gnm EXECUTOR (crates/gnm/src/exec.rs, currently a skeleton) to ACT on a present/sync SUBSET of PM4 — NO draws, NO shaders yet: (1) SubmitAndFlip → drive the EXISTING softgpu present path (the GpuBackend::present landed in task-25; the videoout framebuffer flip already works — wire the Gnm submit-and-flip to it so a PM4-driven guest presents through the same path softgpu uses). (2) EOP/EOS event-write packets (IT_EVENT_WRITE_EOP/EOS) → signal the equeue / label the guest waits on (GPU→CPU sync; see doc-2 C2 timeline model — for now a synchronous label write is fine, no async GPU thread). (3) add a RunCommandList/submit message on the display-thread channel if needed so the executor reaches the backend (doc-2 §3 thread boundary: display thread owns the device, executor ships BackendCmd over crossbeam). Present path from task-25 is REUSED not replaced. ExecMode: this is the PresentSubset mode between TraceOnly (task-21) and Draw (task-24). Validation needs a GPU (present) — the maintainer runs it (LD_LIBRARY_PATH=/usr/lib); the Tier A path of examples/ps4-pm4-test (clear+SubmitAndFlip) is the corpus. NON-GOAL: draws, embedded shaders, pipelines, resource cache (task-24); GCN (phase 4).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 SubmitAndFlip in a PM4 stream drives the existing softgpu present path (GpuBackend::present); a Tier-A PM4 guest (examples/ps4-pm4-test) presents a cleared frame — maintainer-verified live with a Vulkan driver
- [x] #2 IT_EVENT_WRITE_EOP/EOS packets signal the guest's equeue/label (GPU→CPU sync); a guest that submits then waits on the EOP label proceeds
- [x] #3 executor reaches the backend over the display-thread channel per doc-2 §3 (no new async GPU thread); present path from task-25 reused, not duplicated
- [x] #4 six examples oracle unchanged (env-independent); ps4-gnm stays Vulkan-free (executor takes &mut dyn GpuBackend); clippy -D warnings + fmt + cargo test clean
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Present-sink seam: new Vulkan-free trait PresentSink in ps4-core::gpu (submit_and_flip + signal_eop label helper is separate — EOP writes guest mem directly). Registered via OnceLock/RwLock bridge register_present_sink/present_sink() mirroring register_kernel/get_kernel, because the ash backend lives on the display thread and can't be handed to the guest-thread submit handler. Impl PresentSink for GpuManager in ps4-gpu (drives existing submit_flip block-until-vsync handshake — present path from task-25 REUSED). ps4-gnm exec.rs: Executor::run walks decoded PM4 (reusing task-21 decode_submit_range), ExecMode::PresentSubset; on flip submit -> present_sink.submit_and_flip(); on IT_EVENT_WRITE_EOP/EOS -> parse addr+value from packet body (GFX6 layout) and write label to guest mem via identity-mapped raw ptr. Hook exec into record_submit in libscegnmdriver/mod.rs beside trace_submit_range, gated so it only fires when a sink is registered (headless oracle unaffected). Wire register_present_sink(gpu_manager) in app/main.rs. New trace behind debug!/existing PM4_TRACE env so oracle baselines stay clean. Unit-test EOP label write (AC#2) and executor present dispatch with a mock sink (AC#3). AC#1 present-live awaits maintainer.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented (worktree, uncommitted for maintainer review).

DESIGN — present-sink seam:
- New Vulkan-free trait ps4_core::gpu::PresentSink { submit_and_flip(&self) } + OnceLock-style RwLock bridge register_present_sink()/present_sink(), mirroring register_kernel/get_kernel. Chosen over passing &mut dyn GpuBackend because the ash backend physically lives on the display thread and cannot be handed to the guest-thread submit handler (doc-2 §3). The executor names only PresentSink, never GpuManager/ash — ps4-gnm stays Vulkan-free (cargo tree grep empty).
- impl PresentSink for GpuManager (ps4-gpu) drives the EXISTING submit_flip block-until-vsync handshake (GpuCommand::SubmitFlip -> display thread -> backend.present). Present path from task-25 REUSED, not duplicated. No RunCommandList variant needed for present-only phase 3 (doc-2 §3 sanctioned reuse of the existing handshake).
- Deviation from AC#4 literal wording '&mut dyn GpuBackend': the executor takes &dyn PresentSink instead. GpuBackend is the display thread's device handle; the present/sync surface that crosses the thread boundary is a distinct Vulkan-free trait. Spirit honored (Vulkan-free trait, no ash in ps4-gnm); documented in gpu.rs.

EOP/EOS label write:
- exec.rs write_eop_label/write_eos_label parse GFX6 body layout: EOP body [event_cntl, addr_lo, data_cntl(addr_hi in [15:0]), data_lo, data_hi]; EOS body [event_cntl, addr_lo, cmd(addr_hi in [15:0]), data]. Address assembled from addr_lo | (addr_hi<<32); label written to identity-mapped guest address via raw *mut u64 write_unaligned (guest ptr == host ptr, doc-2 §1) — same access model as the decoder. Zero address ignored (interrupt-only EOP). Truncated body -> decoder yields Truncated (not Type3) -> arm skipped, never fatal.

WIRING:
- record_submit (libscegnmdriver/mod.rs) runs Executor::run when present_sink() is Some. Headless (no display thread, no sink) skips it -> oracle baselines unchanged, no new unconditional log lines. app/main.rs registers gpu_manager as the sink at boot.

VERIFICATION (all clean): cargo build --release OK; cargo test all pass incl 7 new exec tests; clippy -D warnings clean; fmt --check clean; run_examples.sh check 6/6 OK env-independent; cargo tree -p ps4-gnm | grep ash/winit/vulkan EMPTY.

ACs: #2 ticked (EOP/EOS unit-tested), #3 ticked (channel reuse + tests, no async thread), #4 ticked (oracle/Vulkan-free/clippy/fmt/test). #1 LEFT UNCHECKED — live present of a Tier-A cleared frame needs a Vulkan driver; awaits maintainer live-verify (LD_LIBRARY_PATH=/usr/lib). Structurally correct + wired; Tier A registers no videoout buffer and emits no EOP, so present presents CURRENT_TARGET and the EOP path is exercised by unit tests, not the corpus.
<!-- SECTION:NOTES:END -->

## Comments

<!-- COMMENTS:BEGIN -->
created: 2026-07-11 13:42
---
Live-verified 2026-07-11 (maintainer, GPU): ps4-pm4-test Tier A SubmitAndFlip presents via GpuBackend::present, guest proceeds past the flip (no headless-style hang) — ZERO-COPY present path. AC#1 confirmed.
---
<!-- COMMENTS:END -->
