---
id: TASK-19
title: >-
  Speed up ps4-syscalls build: replace 94k-arm nid() match with sorted-slice
  binary search
status: Done
assignee: []
created_date: '2026-07-10 11:19'
updated_date: '2026-07-10 13:45'
labels:
  - perf
dependencies: []
priority: medium
ordinal: 19000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
generated_syscalls.rs is ~472k lines; the const fn SyscallId::nid() alone is a 94431-arm match self.0 { id => nid }. Under release opt-level=3 LLVM builds an optimized decision tree over 94k basic blocks — the dominant ps4-syscalls compile cost. TASK-19-adjacent profile override (opt-level=0 in release, done in Cargo.toml) mitigates it, but the structural fix is to drop the match. nid() has a single runtime caller (crates/kernel/src/hle.rs:47, cold symbol-resolution path), not a const context, so it need not be a const fn. Emit a sorted 'static MAP_ID_TO_NID: &[(u64, &str)]' from build.rs (mirroring the existing MAP_BY_ID/NAME/NID slices) and make nid() a binary_search_by_key over it — data lands in rodata (compiles ~instantly) instead of 94k code blocks. Benefits dev and release, even at opt-level>0.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 nid() implemented as binary_search over a sorted static slice; the 94k-arm match is gone from generated_syscalls.rs
- [x] #2 SyscallId::nid() semantics unchanged: hle.rs:47 resolves identical NID strings for all syscall ids (spot-check a few + unknown id returns "")
- [x] #3 ps4-syscalls release compile time measurably drops vs the opt-level=0-override baseline
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. build.rs: remove the 94k-arm nid() match; emit a sorted static MAP_ID_TO_NID: &[(u64,&str)] from entries (already sorted by id).
2. syscalls.rs: add non-const nid() using binary_search_by_key over MAP_ID_TO_NID (returns "" for unknown), matching prior semantics.
3. Verify entries sorted by id; spot-check nid() values unchanged incl unknown-id => "".
4. Measure ps4-syscalls release compile time before/after (touch build.rs to force regen, time it).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-10.
DONE:
- build.rs: replaced the 94431-arm 'match self.0 { id => nid }' with a sorted 'static MAP_ID_TO_NID: &[(u64,&str)]' emitted from entries (already sorted by id at build.rs:137). generated_syscalls.rs no longer contains any match arms.
- syscalls.rs: nid() is now a non-const binary_search_by_key over MAP_ID_TO_NID (returns "" for unknown ids). Dropped 'const' (single runtime caller: kernel/src/hle.rs:47, cold symbol-resolution path).
- Cargo.toml opt-level=0 override for ps4-syscalls LEFT IN PLACE (removing it is a later decision).

VERIFICATION:
- AC#1: generated file has 0 'const fn nid' and 0 '=> "' match arms; MAP_ID_TO_NID slice present.
- AC#2: temporary test (added then removed) iterated all ~94k ids: nid() non-empty for every known id and round-trips through from_nid() back to the same id; unknown ids (u64::MAX, 999999999) return "". PASS.
- AC#3 COMPILE TIME (clean build of ps4-syscalls -p, release, opt-level=0 override active, same machine/devshell):
    BASELINE (old 94k-arm match): 1m48s (108s)
    AFTER   (MAP_ID_TO_NID slice): 27.9s
    ~4x faster, ~80s saved. Benefits dev+release even at opt-level>0.
- cargo build --release green; clippy --all-targets --all-features -D warnings clean; cargo test green (run_guest 9, vm_backend 7, loader 3).
- Oracle run_examples.sh check: only divergence is the known headless env line (Failed to initialize Vulkan: Unable to find a Vulkan driver) on display examples; guest stdout identical. Not a regression.

Status left In Progress for maintainer to set Done after merge.
<!-- SECTION:NOTES:END -->
