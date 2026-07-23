---
id: TASK-25
title: 'gpu: extract GpuBackend trait (present-only) + carve ps4-gnm crate skeleton'
status: Done
assignee: []
created_date: '2026-07-10 19:03'
updated_date: '2026-07-11 06:34'
labels:
  - refactor
dependencies: []
priority: high
ordinal: 25000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
PREREQUISITE for task-20 (see doc-4 §7 + §(a), decision-4). Establish every GPU seam BEFORE the first PM4 line, with ZERO behavior change. Two moves: (1) introduce a narrow GpuBackend trait in ps4-core covering only what the present path needs today — present(target), try_import_host_range(host_ptr,size)->Option<ResourceId>, plus create_target/create_resource/upload stubs; move the current softgpu blit + task-18 zero-copy import behind an AshBackend impl in ps4-gpu; make run_display_loop drive backend.present(target) instead of open-coding fence->acquire->fb-copy->record->submit->present. (2) create the empty Vulkan-FREE ps4-gnm crate with pm4/{decode,opcodes,trace}.rs, exec.rs, state.rs, shader/{source,embedded}.rs, cache/mod.rs skeletons and the ShaderProvider / ResourceCache / DirtySource trait stubs (in ps4-core). No wiring, no PM4 logic yet — pure scaffolding that compiles. Dependency direction per doc-4: core <- gnm <- gpu(ash), core <- gcn(empty) <- gnm; ps4-gnm/ps4-gcn never touch ash/winit. Rationale: without this, task-20..24 get written against VulkanContext's inlined present + one-big-struct state, forcing a retrofit of the trait through PM4 code that already assumes raw ash.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 GpuBackend trait defined in ps4-core (present + try_import_host_range implemented; target/resource/upload stubs); no vk::* type crosses into ps4-gnm
- [x] #2 AshBackend in ps4-gpu implements it; run_display_loop calls backend.present(); softgpu still renders at 60fps with the same zero-copy/staging fallback (task-18 behavior unchanged), verified live by the maintainer
- [x] #3 New Vulkan-free ps4-gnm crate compiles with the pm4/exec/state/shader/cache module skeletons + ShaderProvider/ResourceCache/DirtySource trait stubs; cargo test + clippy -D warnings + fmt clean; oracle unchanged
- [x] #4 ps4-gnm and ps4-gcn have no ash/winit dependency (verified in Cargo.toml); dependency direction core<-gnm<-gpu holds
<!-- AC:END -->





## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Two moves, ZERO behavior change (doc-4 §7 step 1 + §2 + §(a)):
1. GpuBackend trait + Id/desc/error types in ps4-core (present + try_import_host_range implemented; create_target/create_resource/upload stubs). AshBackend impl in ps4-gpu wraps current VulkanContext present path (record_command_buffer blit + task-18 try_import_host_buffer). run_display_loop calls backend.present(target). No vk::* leaves ps4-gpu.
2. New Vulkan-free ps4-gnm crate skeleton (pm4/{decode,opcodes,trace}.rs, exec.rs, state.rs, shader/{source,embedded}.rs, cache/mod.rs) + ShaderProvider/ResourceCache/DirtySource trait stubs (loc per doc-4 §1). Empty ps4-gcn crate (core-only dep). Deps: core<-gnm<-gpu, core<-gcn<-gnm; gnm/gcn never touch ash/winit.
Verify: build+test+clippy -D warnings+fmt clean; oracle diff. softgpu 60fps LIVE = maintainer AC#2 (headless can't verify). Delegated to opus subagent.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
SPEC = doc-4 §7 step 1 + §(a) (read doc-4 fully first — it is the GPU architecture spec; decision-4 is the commitment). This is the NEXT coding task, blocking task-20. Maintainer's call 2026-07-10: task filed, run on his signal after a compaction. ZERO behavior change is the acceptance bar — softgpu must still render at 60fps with task-18 zero-copy/staging fallback intact, maintainer-verified live. Delegate implementation to an opus subagent. See memory gpu-roadmap-order for the full phase sequence.

---
Session 2026-07-10 (opus subagent). LANDED on branch `refactor/gpu-backend-trait` (NOT committed — left in working tree for maintainer review).

Move 1: `GpuBackend` trait + Id/desc/error types in `crates/core/src/gpu.rs` (present + try_import_host_range implemented; create_target/create_resource/upload stubs — create_* return fresh ids, upload is `todo!`). `AshBackend` in new `crates/gpu/src/backend.rs` owns `VulkanContext` + the display-side GPU state (buffers/imported maps, current_target, pending vsync signal) and runs the exact relocated present chain (fence→acquire→fb copy/import→record→submit→present) with task-17/18 two-point vsync-signal timing byte-for-byte preserved. `record_command_buffer` moved verbatim into backend.rs. `run_display_loop` (display.rs, 455→~180 lines) is now a thin winit loop: RegisterBuffer→backend.register_buffer, SubmitFlip→backend.submit_flip, RedrawRequested→backend.present(CURRENT_TARGET). No vk::* leaves ps4-gpu.

Move 2: new Vulkan-free `ps4-gnm` (`crates/gnm`) with driver.rs, pm4/{decode,opcodes,trace}.rs, exec.rs, state.rs, shader/{source,embedded}.rs, cache/mod.rs skeletons; empty `ps4-gcn` (`crates/gcn`, core-only dep). Trait stubs: ShaderProvider in gnm/shader/source.rs, ResourceCache + DirtySource in gnm/cache/mod.rs (no impls). Deps: gpu gains ps4-gnm; gnm deps core+gcn; gcn deps core only. gnm/gcn have NO ash/winit (verified via `cargo tree` — Vulkan-free transitively too).

DEVIATIONS from doc-4 / task-desc:
- DirtySource/ResourceCache/ShaderProvider placement: task-desc parenthetically said "in ps4-core"; doc-4 §1 module tree places them in ps4-gnm. Followed doc-4 §1 (per task-25 instruction): ShaderProvider→gnm/shader/source.rs, ResourceCache+DirtySource→gnm/cache/mod.rs. Noted in cache/mod.rs module doc. doc-4 §8.3 still calls DirtySource a future ps4-core trait wired at runtime; that home is decided when a real impl lands.
- `GpuBackend` trait has NO `Send` bound (doc-4 §2 sketch shows `: Send`). The sole impl AshBackend holds VulkanContext + ImportedBuf (raw ptrs) → not Send, and today it lives solely on the display thread that owns the device (never crosses a thread). Adding Send now would need speculative unsafe impls for zero benefit (§(b) trap #1). Send belongs with the §3 channel-crossing executor and is added then. Documented in the trait doc.
- fence-wait failure now returns `GpuError::Present` (logged by display loop) instead of `.unwrap()` panic — surfaces through the trait's Result; success path unchanged.

VERIFICATION (headless devShell, no Vulkan ICD): `cargo build --release` green; `cargo test` 19 passed (25 suites); `cargo clippy --all-targets --all-features -- -D warnings` exit 0, no lints (9 "warnings" are pre-existing ps4-syscalls build.rs OpenOrbis-SDK-not-found notices); `cargo fmt` clean, no drift. Oracle `scripts/run_examples.sh check`: pass/fail set BYTE-IDENTICAL to `main` (verified via a `main` worktree) — the only divergence is `ps4_gpu::display: Failed to initialize Vulkan: Unable to find a Vulkan driver`, a headless-env artifact present on main too, from the same module. AC #1/#3/#4 ticked. AC #2 (softgpu renders at 60fps LIVE with task-18 zero-copy/staging fallback) is UNCHECKED — cannot be verified headlessly; awaits maintainer live check. Structural confirmation: present path is the same fence/acquire/blit/submit/present order + same zero-copy-vs-staging fallback + same two-point vsync signal, relocated not rewritten.

MERGED to main (fast-forward, commit 19a1963) on maintainer signal "scal oba" 2026-07-10; merged main rebuilt green (build+test+clippy exit 0, both task-23/25 features coexist). Status set Done.

AC #2 CONFIRMED LIVE by maintainer 2026-07-11: `UNEMUPS4_NO_EXTMEMHOST=1 LD_LIBRARY_PATH=/usr/lib cargo run --release -p unemups4 -- examples/ps4-softgpu/ps4-softgpu.elf` renders correctly. NO_EXTMEMHOST=1 forces the task-18 STAGING fallback (the MoltenVK-portable path), so the relocated present chain is verified through the portable-subset path. All 4 ACs met. task-25 fully closed.
<!-- SECTION:NOTES:END -->
