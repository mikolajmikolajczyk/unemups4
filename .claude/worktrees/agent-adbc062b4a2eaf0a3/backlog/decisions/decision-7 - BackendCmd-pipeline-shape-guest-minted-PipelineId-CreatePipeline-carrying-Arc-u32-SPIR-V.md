---
id: decision-7
title: >-
  BackendCmd pipeline shape: guest-minted PipelineId + CreatePipeline carrying
  Arc<[u32]> SPIR-V
date: '2026-07-12 08:39'
status: proposed
---
## Context

The `ShaderProvider` chain is "the SINGLE route for all binds" (doc-4 §4). The
executor now resolves every draw's VS/PS through one injected `&dyn ShaderProvider`
(a `ChainProvider` composite), so the phase-4 GCN provider is *added* to the chain
rather than special-cased into `dispatch_draw_auto`. That threading is done; what is
NOT yet decided is what crosses the display-thread channel when the resolved shader
is recompiled SPIR-V rather than a firmware-embedded id.

Today the embedded draw arm emits `BackendCmd::BindEmbeddedPipeline { vs_id, ps_id }`
+ `BackendCmd::DrawAuto` — the executor sends only embedded *ids*, and the display
thread synthesizes/caches the hardcoded host pipeline keyed by those ids. When the
GCN provider lands (phase-4 keystone), the resolved `HostShader` carries
`Arc<[u32]>` SPIR-V words, and *that SPIR-V* — not an id the backend can synthesize
from — must reach the display thread. Deciding this shape now (cheap) avoids
discovering it mid-keystone, where the temptation is to bolt a second special case
into the draw arm under the pressure of the real-shader triangle.

Two constraints from the existing design fix the shape:

- The channel is **one-way, fire-and-forget** (doc-4 §3): the guest-thread executor
  holds only a `&dyn PresentSink` and cannot round-trip a backend-minted handle
  back. This is exactly why `ResourceId` is minted **guest-side** (see
  `GpuBackend::create_resource` / the `CreateBuffer`/`UploadBuffer` variants), and
  the guest-mint pattern is already carried through the resource path.
- `BackendCmd` **already lost `Copy`**: `UploadBuffer` carries an `Arc<[u8]>`
  snapshot, so the enum is `Clone`-not-`Copy`. Carrying `Arc<[u32]>` SPIR-V on a
  pipeline variant is consistent with that and adds no new constraint.

## Decision

The pipeline crosses the channel as data, mirroring the resource-cache shape:

- **Guest-minted `PipelineId`.** Introduce an opaque `PipelineId(u32)` (in
  `ps4_core::gpu`, next to `ResourceId`/`TargetId`), minted **guest-side** from a
  monotonic counter by the `ps4-gnm` draw/cache path — the same rationale as
  `ResourceId`: a fire-and-forget send cannot return a backend-minted id. The
  display thread records `id -> vk::Pipeline` in its own map.

- **A future `BackendCmd::CreatePipeline` variant** carrying the recompiled SPIR-V,
  shape roughly:

  ```rust
  CreatePipeline {
      id: PipelineId,           // guest-minted
      vs_spirv: Arc<[u32]>,     // recompiled, spirv-val-gated, portable (decision-3)
      ps_spirv: Arc<[u32]>,
      // + pipeline-state fields derived from the register banks (doc-4 §5/§C8),
      //   grown one-per-milestone exactly like BackendCmd/GpuBackend already do.
  }
  ```

  `Arc<[u32]>` is the exact payload `HostShader.spirv` already holds, so the provider
  hands its resolved words straight onto the command with no re-encode. `BackendCmd`
  stays `Clone`-not-`Copy` (already true); match on `&BackendCmd`.

- **Draw binds the pipeline by id.** The draw arm emits `CreatePipeline { id, .. }`
  on a **cache miss** and then a bind-by-id (a `BindPipeline { id }`, the SPIR-V
  variant of today's `BindEmbeddedPipeline`), so steady-state draws ship only the
  small id, never the SPIR-V again.

- **The pipeline cache lives display-side, keyed by `PipelineId`.** The guest-side
  cache tracks id assignment (get-or-mint per pipeline key); the display thread owns
  the `id -> vk::Pipeline` map and the actual `vk::` objects — the executor never
  names a `vk::` type, preserving the Vulkan-free boundary (doc-4 §1).

- **The channel stays fire-and-forget.** No handshake, no return path — the id is
  the shared name both sides agree on, exactly as for buffers.

NOT implemented in this task: `PipelineId`, the `CreatePipeline`/`BindPipeline`
variants, and the display-side cache all land with the phase-4 recompiler wiring
(the GcnShaderProvider + per-submit draw work). This decision only *records* the
shape and threads the provider chain so those follow-ups add a provider + a variant,
not a rewrite. `crates/core/src/gpu.rs` is intentionally left unchanged here.

## Consequences

- The follow-up work that adds `CreatePipeline` must agree with this shape:
  - the per-submit command-list / draw work (adds the `CreatePipeline` +
    bind-by-id emission on cache miss), and
  - the `GcnShaderProvider` (feeds `HostShader.spirv` straight onto the command).
- Ties into decision-6 (phase-4 architecture: "SPIR-V + texture data cross the
  existing BackendCmd/RunCommandList channel; BackendCmd loses Copy, carries
  Arc<[u32]> on pipeline-cache miss") — this decision is the concrete BackendCmd
  shape of that commitment — and to doc-4 §4 (the single provider route) + the
  guest-minted-id rationale documented on `GpuBackend::create_resource`.
- Non-goal here: pipeline-state field enumeration (blend/depth/raster from the
  register banks) — those fields are grown one-per-milestone with the state model
  (doc-4 §5/§C8), not fixed now.
