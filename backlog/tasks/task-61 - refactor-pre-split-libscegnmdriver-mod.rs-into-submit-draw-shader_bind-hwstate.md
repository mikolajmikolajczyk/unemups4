---
id: TASK-61
title: >-
  refactor: pre-split libscegnmdriver/mod.rs into
  submit/draw/shader_bind/hwstate
status: Done
assignee: []
created_date: '2026-07-11 14:47'
updated_date: '2026-07-11 15:31'
labels:
  - libs
dependencies: []
priority: medium
ordinal: 60000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/libs/src/libscegnmdriver/mod.rs is ~630 lines / ~30 NID handlers in one flat file, mixing submit/flip, draw/dispatch builders, HW-state-init stubs, markers, and 8 sceGnmSet*Shader binds (:374-497). Phase 4 turns the LS/HS/ES/GS/CS stub binds into real .sb bind paths into state.rs — the file grows along a clear seam. Mechanical, behavior-preserving split into libscegnmdriver/{submit.rs,draw.rs,shader_bind.rs,hwstate.rs} (#[ps4_syscall]+inventory registration is location-independent). Pre-phase-4, before the binds get fleshed out.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 handlers split into cohesive submodules; mod.rs is thin re-export/glue
- [x] #2 behavior-preserving: all NIDs still register+resolve (key_gnm_nids test passes), zero logic change
- [x] #3 tests + clippy -D warnings + fmt + oracle green
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Baseline: build+tests green (done). 2. Create submodules under libscegnmdriver/: submit.rs (Submit/AndFlip/Done + record_submit + read_u64/u32_array), draw.rs (DrawIndex/Auto/DispatchDirect/MapComputeQueue/DingDong), shader_bind.rs (SetEmbeddedVs/Ps + SetVs/Ps/Cs/Es/Gs/Hs/Ls), hwstate.rs (DrawInitDefaultHardwareState*/AreSubmitsAllowed/Insert*Marker/InsertWaitFlipDone/FlushGarlic). 3. mod.rs: thin - pub mod decls + #[cfg(test)] tests. 4. Pure code-move: only use/visibility adjusts, zero logic edit. record_submit+readers made pub(super) or pub(crate) for reachability. 5. Verify: build, test (all green incl key_gnm_nids + submit_handler), clippy -D, fmt --check, run_examples check 6/6.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-11. Pure behavior-preserving code-move (zero logic change). Split libscegnmdriver/mod.rs (26 #[ps4_syscall] handlers) into 4 cohesive submodules; mod.rs is now thin (pub mod decls + module doc + #[cfg(test)] tests):
- submit.rs (3): SubmitCommandBuffers / SubmitAndFlip / SubmitDone + record_submit helper + read_u64_array/read_u32_array (task-59 IdentityMem.read_array delegation preserved verbatim).
- draw.rs (5): DrawIndex / DrawIndexAuto / DispatchDirect / MapComputeQueue / DingDong.
- shader_bind.rs (9): SetEmbeddedVs/PsShader (incl. bind_embedded_shader calls) + SetVs/Ps/Cs/Es/Gs/Hs/LsShader.
- hwstate.rs (9): DrawInitDefaultHardwareState{,175,200,350} + AreSubmitsAllowed + InsertPush/PopMarker + InsertWaitFlipDone + FlushGarlic.
Only edits beyond code-move: per-file use-imports (each handler file needs use crate::context::NativeContext for the macro-generated wrapper; submit.rs keeps MemoryAccessExt for read_array; tests import moved handlers explicitly). No handler renamed, no signature/NID/logic touched. #[ps4_syscall]+inventory registration is location-independent -> all 26 still register.
Verify: cargo build --release OK; cargo test 116 passed/3 ignored (incl. all 5 libscegnmdriver tests: key_gnm_nids_resolve_to_registered_handlers + both submit_handler_*); clippy --all-targets --all-features -D warnings 0 errors; cargo fmt --check clean; ./scripts/run_examples.sh check 6/6 match baselines. NOT committed (left for maintainer).
<!-- SECTION:NOTES:END -->
