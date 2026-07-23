---
id: TASK-80
title: >-
  gnm/core: narrow parse_sb read seam to BoundedRead trait — prevent unbounded
  guest read at task-53
status: Done
assignee: []
created_date: '2026-07-12 07:54'
updated_date: '2026-07-12 08:19'
labels:
  - gpu
  - gnm
  - core
dependencies: []
priority: medium
ordinal: 79000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fable phase-4 quality review finding #2 (ARCHITECTURE). crates/gnm/src/shader/sb.rs:425 (parse_sb) + sb.rs:455-509 (1 MiB windowed magic scan) + crates/core/src/bounded_read.rs (BoundedRead) + crates/memory/src/vm_backend.rs (VmaBoundedView) + crates/gnm/src/exec.rs:190-224 (resolver uses IdentityMem). parse_sb takes a full &dyn VirtualMemoryManager, but the VMA-bounded reader the guest thread can actually reach (bounded_read(), Arc<dyn BoundedRead>) is NOT a VirtualMemoryManager; VmaBoundedView is, but lives in ps4-memory which ps4-gnm doesn't/shouldn't depend on, and the executor holds no handle to it. So task-53's path of least resistance = pass IdentityMem → parse_sb's magic scan reads up to 1 MiB of raw host memory from a guest-controlled PGM_LO/HI addr: the exact over-read class bounded_read killed, reintroduced ONE seam over. FIX (before task-38 copies parse_sb's plumbing): parse_sb/parse_vs_semantics/parse_ps_semantics only ever call read_bytes — make them take a small reader trait (ps4_core::bounded_read::BoundedRead is already the exact shape) with a blanket impl for &dyn VirtualMemoryManager. Then the executor feeds bounded_read() straight into parse_sb, no new dep. BONUS: deletes 3 near-identical ~50-line VirtualMemoryManager stub impls that exist only to satisfy 8 unused trait methods in tests — BufMem (sb.rs:569-625), BoundedTestMem (bounded_read.rs:88-144), OneRegionMem (shader_bind.rs:455-510).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 parse_sb + semantics parsers take a BoundedRead-shaped reader (not the full VirtualMemoryManager); blanket impl for &dyn VirtualMemoryManager keeps existing callers working
- [ ] #2 the 3 test stub mems collapse to the minimal reader trait (no more 8-method VirtualMemoryManager boilerplate per test)
- [ ] #3 a parse_sb over a guest-controlled PGM addr through the executor path cannot issue an unbounded read (the reader is the bounded seam); regression test
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
DONE 2026-07-12 (feat/task-80 @<prior-history>, merged). parse_sb + helpers (read_exact/scan_for_magic/read_shrinking/parse_vs_semantics/parse_ps_semantics) now take mem: &(impl BoundedRead + ?Sized), read via read_ranged (routes to read_bytes_ranged, NEVER unbounded read_bytes). Blanket impl<T: VirtualMemoryManager + ?Sized> BoundedRead for T (bounded_read.rs) keeps VMM callers working (no coherence conflict with Arc<RwLock<Box<dyn VMM>>> impl — Arc isn't a VMM). PAYOFF: even bare IdentityMem handed to parse_sb now rejects cleanly (its read_bytes_ranged is the fail-loud Err default) — over-read class closed one seam over. Executor: parse_sb_bounded(code_start) — Some(src)=>parse_sb(code_start, src.as_ref()); None=>Err(MemoryFault) headless (strictly refuses unbounded read, no IdentityMem fall-through). #[allow(dead_code)] = phase-4 draw path ready entry. STUBS DELETED (~215 lines): BufMem (sb.rs), BoundedTestMem→RegionReader (bounded_read.rs), OneRegionMem (shader_bind.rs), BufMem (corpus_load.rs). Regression: parse_sb_bounded_over_guest_pgm_addr_cannot_over_read (counting reader proves no read >=1MiB slipped through, headless=MemoryFault). gnm/Cargo.toml dev-dep ps4-core test-hooks. TASK-53/38: use parse_sb_bounded(code_start), NEVER pass IdentityMem to parse_sb. Verify: workspace 187 pass, clippy 0, fmt clean, gnm Vulkan-free. Combined gate: 29 suites, oracle 6/6.
<!-- SECTION:NOTES:END -->
