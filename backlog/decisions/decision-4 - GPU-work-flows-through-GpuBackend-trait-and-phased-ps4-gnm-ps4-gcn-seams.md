---
id: decision-4
title: GPU work flows through GpuBackend trait and phased ps4-gnm/ps4-gcn seams
date: '2026-07-10 18:57'
status: accepted
---

- **Status:** Accepted
- **Deciders:** Mikołaj Mikołajczyk

## Context

`decision-3` set the GPU direction (Bloodborne north star; phased path
present → Gnm boot+trace → PM4 present/sync → embedded-shader draw → GCN
recompiler; ash primary + MoltenVK/Metal portability; interp-as-oracle). It did
**not** fix the *architecture* the phases plug into. Today the GPU is one
`crates/gpu` crate with an inlined, hardcoded present pipeline (`VulkanContext`
one-big-struct + `record_command_buffer`) and a two-message channel. Starting
the PM4 work directly against that would calcify raw `ash` across the
command processor and force a rewrite at each phase.

`doc-2 - GPU-subsystem-architecture.md` designs the target architecture and the
seams. This decision records the cross-cutting commitments it implies so future
sessions don't re-litigate them.

## Decision

1. **All GPU work flows through a `GpuBackend` trait in `ps4-core`.** The trait
   captures *what we ask the GPU to do* at PS4/PM4 granularity
   (present / create+upload / try-import / draw / bind / target / signal-eop),
   not raw Vulkan verbs. `ash` is the sole impl (`AshBackend` in `ps4-gpu`);
   MoltenVK is the same impl on the Vulkan portability subset; a future native
   Metal backend is an alternative impl. No `ash::vk` type may appear in the
   command-processor code. The trait is introduced **now** with only the
   present/import surface implemented and the rest sketched; it grows one method
   per phase. No second backend impl, and no command-encoder/render-graph/
   allocator abstraction, is built speculatively.

2. **The command processor lives in a Vulkan-free `ps4-gnm` crate; GCN shader
   translation in a separate `ps4-gcn` crate (which holds the GCN ISA decoder,
   the CPU wave interpreter that serves as the correctness oracle, and the
   GCN→SPIR-V recompiler).** Dependency
   direction is strictly one-way: `core ← gnm ← gpu(ash)` and `core ← gcn ← gnm`.
   `ps4-gnm`/`ps4-gcn` never depend on `ash`/`winit`/`ps4-gpu`; they read guest
   memory via the `ps4-core` `VirtualMemoryManager` trait. The `libSceGnmDriver`
   NID handlers stay thin in `crates/libs` (like `libscevideoout`) and call into
   `ps4-gnm`. This keeps decode / state / cache / GCN logic headless-testable
   (the devShell has no Vulkan driver) behind a `MockBackend`.

3. **One pipeline, phase-gated.** PM4 decode + trace run in every mode; phases
   add executor match-arms (trace-only → present/sync → draw), never forks.
   Guest→GPU data movement is one policy — a resource cache keyed by
   `(guest addr, size, layout)` with zero-copy import (task-18 `external_memory_host`)
   and copy+dirty-track as two points on it; copy+invalidate is the portable
   default (MoltenVK lacks `external_memory_host`). Shaders resolve through a
   `ShaderProvider` seam (embedded-id → hardcoded SPIR-V for the embedded-shader
   path; GCN `.sb` → SPIR-V via the interpret-then-recompile path) so the
   embedded-shader path hardcodes nothing the real GCN recompiler must tear out.

4. **A pre-task-20 refactor task lands first:** extract the present-only
   `GpuBackend` trait and carve the empty `ps4-gnm` skeleton with zero behavior
   change (same present, 60fps, zero-copy/staging fallback). PM4 code is written
   against these seams from its first line.

5. **PS4 hardware constraints get a seam now, implementation later.** The keys/
   traits carry fields for: multi-ring/ACE submission + DCB/CCB/ACB streams;
   a GPU timeline (EOP/EOS/labels → Vulkan timeline semaphores); tiled/swizzled
   layouts (detile-on-upload); GCN descriptor-from-memory bindings (V#/T#/S#);
   HW-stage roles (LS/HS/ES/GS/VS ≠ logical); onion/garlic memory-type policy;
   DCC/HTILE compression; a shadow register file (context/SH/uconfig) with
   derive-pipeline-at-draw. **Readback (GPU→guest) is gated OFF by default** (env
   lever now, per-title later) — upload is always on, readback is the expensive
   reverse direction kept off the hot path. None of these are implemented ahead
   of their phase; only their seams exist.

## Alternatives considered

- **Keep one `crates/gpu` and grow it.** Rejected: PM4 execution would couple to
  raw ash and the one-big-struct state; every phase would fight a rewrite, and
  the pure logic would be untestable without a GPU driver.
- **Full portable GPU-HAL now (command encoder / barriers / render graph).**
  Rejected as over-engineering: modeled on no real second backend. The trait is
  drawn only at the granularity a Metal port would need; MoltenVK needs
  discipline (gate-and-fallback), not new abstraction.
- **Dedicated async GPU thread now.** Deferred: the display
  thread keeps the Vulkan device and replays a `BackendCmd` list sent over the
  existing channel, preserving today's working present path. Revisit if perf
  demands.
- **Reuse x86jit SMC dirty tracking for cache invalidation as-is.** Not possible
  today: x86jit dirty-tracks only pages tagged as *code* (`mark_code` →
  `take_dirty_code`); general data-page writes aren't recorded. A watched-data-
  range dirty API is a needed x86jit capability (its own backlog), with
  conservative per-submit re-upload as the interim stand-in and `mprotect` as a
  documented fallback.

## Consequences

- Positive: raw ash cannot spread into the command processor; ~90% of new GPU
  code is headless-`cargo test`-able; each phase is an increment; portability
  (MoltenVK/Metal) and the hard PS4 constraints have seams, not retrofits.
- Cost: three crates instead of one, and a prerequisite refactor before task-20.
  Deliberate — the refactor is behavior-preserving and small.
- New cross-crate dependency surfaced: a memory-type (onion/garlic) flag threaded
  from `ps4-memory`/kernel into the GPU cache, and a desired x86jit watched-range
  dirty API. Both recorded as open questions in `doc-2`.

## Trigger to revisit

If MoltenVK/Metal is dropped, the `GpuBackend` trait may collapse back toward ash
(keep it only as the headless-test seam). If the Bloodborne north star narrows to
educational homebrew, `ps4-gcn` and the heavier cache/constraint seams can be
re-deferred with `decision-3`.
