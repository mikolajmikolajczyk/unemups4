---
id: TASK-42
title: >-
  gnm: GcnShaderProvider — .sb → recompiled SPIR-V behind ShaderProvider,
  chained after embedded
status: Done
assignee: []
created_date: '2026-07-11 12:54'
updated_date: '2026-07-12 15:00'
labels:
  - gpu
  - gnm
dependencies:
  - TASK-36
  - TASK-40
priority: medium
ordinal: 41000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
§4 second provider. Impl ShaderProvider in gnm calling ps4-gcn: GcnBinary{addr}→P4-01 parse→P4-05 recompile→HostShader (SPIR-V + I/O layout + HW-stage role §C8), cached by shader hash (m_shaderHash0/1 + length) so re-binds don't re-recompile. Executor resolve becomes a real chain [EmbeddedShaderProvider, GcnShaderProvider] (embedded keeps precedence; today's Err(ShaderUnsupported) defer becomes a resolve). Unrecompilable shaders defer cleanly, log naming the instruction. Does NOT change draw-arm state sourcing (P4-09/P4-18).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 headless: GcnBinary ref over mock memory holding a corpus blob resolves to valid SPIR-V through the chain; embedded ids still resolve as today (existing tests pass)
- [x] #2 headless: resolve cached — 2nd resolve of same hash skips recompiler (counter/test hook)
- [x] #3 headless: blob with unsupported instr defers with structured reason, no crash
<!-- AC:END -->



## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Fable phase-4 quality review finding #5 (2026-07-12) — recompile cache seam: GcnShaderProvider should own a code_addr -> Arc<HostShader> cache, invalidated via the SAME DirtySource watch mechanism the buffer cache (task-49) uses (watch code_range, invalidate on dirty). parse_sb is per-call EXPENSIVE (up to 256 windowed reads, each a Vec alloc; against a VMA-bounded view each failed read is a VMA walk) and derive_bound_shaders runs PER DRAW — naive task-53 wiring = re-scan + re-parse + re-recompile per draw. The .sb ShaderBinaryInfo header already carries perfect cache-key material (shader_hash0/1, crc32, + code addr). Keep task-38/39 parse output cheaply shareable (Arc<HostShader>) so this cache is a drop-in. Coordinate with task-40 (recompiler).

Done (2026-07-12). Landed `crates/gnm/src/shader/gcn.rs::GcnShaderProvider`:
- `resolve(GcnBinary{addr})` → `parse_sb` (through the process-global `bounded_read()` seam, NOT the caller's unbounded IdentityMem `mem`) → `decode_all` → `recompile` → `HostShader { spirv: Arc<[u32]>, io: Some(IoLayout), stage }`. `HostShader` gained `io: Option<ps4_gcn::IoLayout>` (None for embedded).
- Hash-keyed skip: cache key = (shader_hash0, shader_hash1, crc32, code_len) → `Arc<HostShader>`; a 2nd resolve of the same hash is a refcount bump, recompiler NOT run. Test hook: `recompile_count()`.
- Defer: parse reject / unmodeled stage / `RecompileError` all return `Err(ShaderUnsupported)` + a `tracing::warn!` naming the reason. Never panics.
- Chain: `submit.rs` now assembles `[EmbeddedShaderProvider, GcnShaderProvider]` (embedded first, keeps precedence); executor is NOT special-cased.
- Invalidation MECHANISM is fully implemented and tested (`drain_dirty(&dyn DirtySource)` + `invalidate_range` + watch-on-insert, mirroring `ResourceCache::drain_dirty`). NOT-YET-WIRED to production: the `ShaderProvider::resolve` trait is `&self` with no DirtySource in reach, so the production resolve path threads `dirty: None` (cache skips, but does not watch). TYPED SEAM for task-53: `GcnShaderProvider::resolve_gcn(addr, reader, dirty: Option<&dyn DirtySource>)` + the public `drain_dirty`/`invalidate_range`. Task-53 makes the provider driver-owned (persistent cache across submits) and calls `drain_dirty` per submit with the threaded dirty source — same shape the resource cache uses.
- ps4-gnm + ps4-gcn stay Vulkan-free (grep clean; matches are comments only).
<!-- SECTION:NOTES:END -->
