---
id: TASK-174
title: >-
  gnm/gpu: support dual VS-CB + PS-CB (two const-storage slots) — unblock
  Celeste title RT producers (task-171 fix)
status: Done
assignee: []
created_date: '2026-07-18 16:37'
updated_date: '2026-07-19 22:18'
labels:
  - gnm
  - gpu
  - celeste
  - recompiler
  - retail
dependencies: []
priority: high
ordinal: 178000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
task-171 root cause: Celeste's title/gameplay RT-producer draws (mountain/parallax into offscreen RT 0x9b00e0000 + bloom RTs) all DEFER because their shaders declare BOTH a VS constant buffer AND a PS constant buffer, colliding on our single set0/bind2 const-storage slot (doc-6 Entry 10 strict-or-defer, crates/gnm/src/exec.rs:506-513). register_render_target runs BEFORE the defer (exec.rs:456) and deferred cmds aren't rolled back (exec.rs:267-275), so each RT is allocated but never rendered -> composite samples undefined memory -> black bg + garbage + atlas-splatter on the title. FIX: give VS-CB and PS-CB two distinct const-storage descriptor slots (keep VS-CB at set0/bind2, add a second binding for PS-CB) threaded through the pipeline layout + BindConstBuffer, instead of deferring. CONFIRMING EXPERIMENT FIRST (confirm-before-implement): env-gated -- when both CBs present, bind only VS-CB and drop PS-CB (don't defer); if the title background fills with the mountain (even if PS color-grade wrong) the defer is confirmed as the cause. Then implement the proper two-slot fix; PNG/live oracle for pixel-correctness. Relates task-171, task-56, doc-6 Entry 10.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Confirming experiment (env-gated VS-CB-only, no defer) shows the title RTs fill with real content instead of black/garbage
- [x] #2 A draw with both VS-CB and PS-CB binds both to distinct const-storage slots (no defer); pipeline layout + BindConstBuffer thread both through
- [ ] #3 Celeste title renders the composited scene cleanly (mountain + composite, no black bg / garbage / atlas-splatter) — PNG/live oracle; build+test+clippy clean
<!-- AC:END -->

## Notes

<!-- SECTION:NOTES:BEGIN -->
### PARKED — confirming experiment INCONCLUSIVE, blocked on input (2026-07-18)
Built the confirming probe `UNEMUPS4_DUAL_CB_VS_ONLY=1` (bind VS-CB, drop PS-CB, don't defer, at exec.rs dual-CB check; default path unchanged; uncommitted in worktree agent-aa461f7fd09ecf828). **Maintainer ran it — "probe changed nothing in this scene."** Reason: the dual-CB defer / offscreen-RT producers are the INTERACTIVE title/menu scene reached by pressing X/confirm on the pad; **our emulator never reaches it** — we loop on the 2D attract screen (CELESTE logo + 2D mountain, direct scanout, no dual-CB). The probe's scene is unreachable, so it can't be visually validated.
**Blocked on: input.** X/confirm is not reaching the guest (see the input task) — until it does we can't advance past attract to the RT scene, so the dual-CB fix (root cause is airtight per task-171) can't be visually confirmed. Sequence: fix input → reach the interactive scene → then validate/implement this two-slot fix. Root cause + fix direction remain valid; this is purely blocked on reachability.

### TWO-SLOT FIX IMPLEMENTED — build/clippy/test green, live visual pending (2026-07-19)
Implemented the proper two-slot dual-CB fix (AC #2). VS-CB stays at set0/bind2; PS-CB now emits at set0/**bind6** — two distinct set-0 STORAGE_BUFFER descriptors, so a draw whose VS AND PS both `s_buffer_load` binds BOTH instead of deferring. Files touched (UNCOMMITTED, worktree agent-a1e2f32826dc3ec1c):
- **recompiler** `crates/gcn/src/recompile.rs`: added `PS_CONST_BUFFER_BINDING = 6`; `ensure_const_buffer` picks binding by stage (Fragment→6, else→2), used in both the SPIR-V `Binding` decoration and the `io.const_buffers` entry. (PS const moves unconditionally — the recompiler compiles each stage in isolation and can't see the other, so the PS slot must always be distinct.)
- **protocol** `crates/core/src/gpu.rs`: `CreatePipeline.const_storage_fragment` changed `bool` → `Option<StorageBinding>` (the PS const slot; `const_storage` stays the VS slot). `ResourceSignature` gained `const_storage_fragment: Option<ResourceSlot>` so a dual-CB layout keys to its own pipeline.
- **backend** `crates/gpu/src/backend.rs`: `create_host_pipeline` emits a second STORAGE_BUFFER dsl binding (FRAGMENT flags) for the PS const; per-record `const_bind: Option<ConstBind>` → `const_binds: Vec<ConstBind>` (accumulates both binds); descriptor-pool sizing + one WriteDescriptorSet per const bind; `needs_const` = either slot.
- **executor** `crates/gnm/src/exec.rs`: the `(Some([_]),Some([_])) => return None` defer is GONE. Resolves each stage's CB V# independently (each from its own user-SGPR block), keys both into `ResourceSignature`, ships both on `CreatePipeline`, emits a `BindConstBuffer` per stage. `>1 CB in one stage` still defers (strict-or-defer preserved, per-stage).
- **tests**: updated `gcn/tests/recompile.rs` (PS const now bind6), the `gcn/tests/spirv_eval` role decoder (binding 6 = Cbuffer, alongside 2), and the `core/gpu.rs` CreatePipeline unit test. `cargo build --release`, `cargo clippy --all-targets --all-features`, and `cargo test` (42 test-result groups) all GREEN.

**Confirming experiment** `UNEMUPS4_DUAL_CB_VS_ONLY` REBUILT (was lost — the old build was uncommitted in a different worktree). It now means "when both stages declare a CB, drop the PS-CB, bind VS only" — a diagnostic to A/B the PS constant's contribution; the DEFAULT path already binds both.

**HEADLESS-PROVEN**: build/clippy/test green; a 45s smoke run reaches the executor with **zero** Vulkan validation errors / panics / segfaults and 0 dual-CB defer messages — no regression on the reachable splash/attract draws (the new two-slot descriptor layout builds valid pipelines).
**NEEDS MAINTAINER'S LIVE EYES** (AC #1, AC #3): the dual-CB RT-producer scene (interactive title/menu, reached by pressing confirm) is STILL unreachable headlessly — without controller input the guest loops in the intro/asset-load (only ~11 draws, all direct-to-videoout, none dual-CB; matches the PARKED note above). DUMP_PNG is untrustworthy for late frames (task-178). So validate LIVE: run the normal build to the title and check the mountain/parallax composite renders cleanly (no black bg / garbage band / atlas-splatter). If clean, this warrants a **doc-6 entry** (mechanism: two-slot const buffers superseding the dual-CB strict-or-defer; supersedes Entry 10 for the dual case).
<!-- SECTION:NOTES:END -->

### Closed 2026-07-20 — landed
Dual VS-CB + PS-CB (two const-storage slots) shipped: PS-CB at set0/bind6, VS-CB at
set0/bind2, defer removed. In use on the Celeste title path (both const buffers bind
per draw); the title screen renders clean after the task-178 cache fix.
