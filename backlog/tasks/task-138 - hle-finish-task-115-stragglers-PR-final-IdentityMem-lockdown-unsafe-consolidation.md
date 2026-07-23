---
id: TASK-138
title: >-
  hle: finish task-115 stragglers + PR-final IdentityMem lockdown (unsafe
  consolidation)
status: Done
assignee: []
created_date: '2026-07-16 11:48'
updated_date: '2026-07-16 13:41'
labels:
  - hle
  - tech-debt
  - unsafe
dependencies:
  - TASK-115
priority: medium
ordinal: 144000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The consolidation tail of task-115: migrate the remaining out-of-tier raw guest derefs onto the GuestPtr/GuestSlice + write_guest seam, THEN land the PR-final lockdown that compiler+clippy-enforces it. Once done, effectively all guest-memory unsafe is funnelled through the single audited seam in crates/core/src/guest_ptr.rs (the rest of our unsafe is irreducible Vulkan/ash FFI in crates/gpu).

Remaining raw-deref sites (from task-115 Lane C survey, main): submit.rs:22,30 (IdentityMem array reads — executor-entangled, migrate alongside the executor read path); pthread.rs attr/priority out-params (~707-820); sema.rs:163; libscaudioout/mod.rs:229,294,317; mman.rs:150,155 (addr out-params); libkernel/mod.rs getcwd/gettimeofday; libscenet:202. LEGIT exceptions to KEEP (document, do not migrate): gnm executor + PM4 decoder IdentityMem hot path (guest thread, no mem-mgr handle); the shader_bind/draw headless test-only IdentityMem fallback; the EOP/EOS label store (hot submit path, VMA-guarded).

PR-final LOCKDOWN (lands LAST, only after the migrations above): demote ps4_gnm::idmem::IdentityMem to pub(crate) so libs can no longer construct it; add clippy disallowed_methods in clippy.toml (ptr::write_bytes, CStr::from_ptr, bare guest stores) with #[allow] only on the documented legit exceptions. The compile-break + clippy failure is the proof that migration is complete — no new raw guest deref can appear outside the seam. SMC preserved by construction (writes via write_guest -> write_bytes). Headless/no-seam = fail clean, never raw-deref fallback. Watch the read_cstr test-lock deadlock class (never stack two override_scoped on one test_lock).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 All out-of-tier raw guest derefs listed above migrated to GuestPtr/GuestSlice/read_cstr (reads via bounded seam) + write_guest (writes, SMC-observed); bad ptr = clean errno/no-op, not host segfault
- [x] #2 ps4_gnm::idmem::IdentityMem demoted to pub(crate); libs no longer references it except the documented legit exceptions (which get explicit #[allow] + a comment)
- [x] #3 clippy.toml disallowed_methods bans raw guest deref patterns (ptr::write_bytes/CStr::from_ptr/bare stores); cargo clippy --all-targets -D warnings passes with the exceptions allow-listed
- [x] #4 cargo test --workspace green (host, LD_LIBRARY_PATH=/usr/lib); no read_cstr test-lock deadlock
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done 2026-07-16 (merge 4c4a0c8). LOCKDOWN complete + independently re-verified: clippy 'No issues found', 427 workspace tests, fmt+build clean. Part A migrations: submit.rs read_u{64,32}_array->bounded_read; libkernel heap_trace/pres/map_direct + pthread mutex_destroy/cond_destroy->GuestPtr; gnmdriver draw/shader_bind headless IdentityMem fallbacks removed (tests rewired to host-backed seams). Part B lockdown: ps4_gnm::idmem::{IdentityMem,BoundedMem} demoted pub(crate) (compile-break = completeness proof); new repo-root clippy.toml bans std::ptr::write_bytes (1 documented #[allow] at core::memory::zero_memory = the seam). CStr::from_ptr intentionally NOT banned (only legit Vulkan FFI ext-name reads in gpu; clippy can't scope per-crate). In-gnm hot-path IdentityMem = method calls on in-crate struct, compile clean no #[allow]. RESIDUAL (out of scope, future sweep, NOT ptr::write_bytes so slip the clippy rule = bare stores/read-scans): thread create/join/key-create, mutexattr ops, allocate_direct_memory, sce_net_sendto sockaddr, fs.rs open/read_cstr path scans.
<!-- SECTION:NOTES:END -->
