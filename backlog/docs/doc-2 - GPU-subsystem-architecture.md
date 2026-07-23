---
id: doc-2
title: GPU subsystem architecture
type: other
created_date: '2026-07-10 18:50'
---

# GPU subsystem architecture

**Status:** LANDED. The design in this doc is built and running: the full GPU path
(PM4 decode → GCN decode → CPU interpret / SPIR-V recompile → Vulkan) executes, and
retail Celeste renders in-game through it (~24–26 fps in-game, ~58 in menu). **Scope:**
the target architecture for the GPU subsystem, from the present-only path through the
increments described in `decision-3` (Gnm boot+trace → PM4 present/sync → embedded-shader
draw → GCN shaders), each of which plugged in as an increment rather than a rewrite.

**Ground truth for this doc** (read these, don't re-derive):
`decision-3` (Bloodborne north star, incremental PM4 path, ash primary + MoltenVK/Metal
policy, interp-as-oracle); `doc-1` (OpenOrbis
Gnm/shader capabilities); the current code —
`crates/gpu/src/{vulkan.rs,display.rs,commands.rs,lib.rs}`,
`crates/libs/src/libscevideoout/mod.rs`, `crates/kernel/src/bridge.rs`; tasks
task-20/21/22/24.

This doc is organized around the seven design questions plus a required eighth
section on unified-memory resource caching. All Rust signatures below are
**illustrative sketches, not final API** — they exist to pin down the *shape* of a
seam, not its exact spelling.

---

## 0. Current state (what we are refactoring from)

- **`crates/gpu`** is a single crate: `VulkanContext` (one large struct holding
  every ash handle — instance/device/swapchain/render-pass/one hardcoded
  present pipeline/one 1920×1080 RGBA texture + staging buffer + the task-18
  zero-copy import machinery), `run_display_loop` (winit event loop that owns the
  Vulkan device and runs the per-frame fence→acquire→fb-copy→record→submit→present
  chain), `GpuManager`/`GpuCommand` (a crossbeam channel with exactly two messages,
  `RegisterBuffer` and `SubmitFlip`), `present_profile` (atomics).
- **Data path today:** guest → `sceVideoOut*` HLE (`crates/libs`) → `KernelBridge`
  (`crates/kernel`, implements the `KernelInterface` trait from `crates/core`) →
  `GpuManager::{register_buffer,submit_flip}` → crossbeam channel → display loop.
  `submit_flip` **blocks the guest thread** until the display loop signals vsync.
- **Thread model (load-bearing for §3):** the guest runs on emulator thread(s);
  the **display loop owns the main OS thread and the entire Vulkan device**. The
  crossbeam channel is the only thread boundary. All Vulkan calls happen on the
  display thread. This is not incidental — winit + most drivers want their event
  loop and device work on one thread.
- **Dependency edges:** `gpu` depends only on `ps4-core` (for the
  `VirtualMemoryManager` trait, `pad`) + ash/winit; nothing depends on `gpu`
  except the app. `libs → core (trait)`, `kernel` provides the concrete bridge.
  Handlers self-register via `#[ps4_syscall]` + `inventory`. Syscall dispatch and
  the fault annotator are wired at runtime through `OnceLock` callbacks to avoid
  crate cycles — **this "trait in core, impl elsewhere, wire at runtime" pattern
  is the template the GPU seams below follow.**
- **Identity mapping:** guest ptr == host ptr (x86jit `guest_base`). The
  `VirtualMemoryManager::get_host_ptr`/`read_bytes` surface in `ps4-core` already
  lets any crate read guest memory with no translation — this is exactly what a
  headless PM4 decoder needs.

Two structural facts drive everything below: **(a)** the present pipeline is
hardcoded and inlined into `record_command_buffer`; there is no notion of "a
pipeline the guest asked for." **(b)** All GPU state lives as fields on one
`VulkanContext` struct. Both must be loosened *before*, not during, the PM4 work,
or PM4 execution will calcify around them.

---

## 1. Module / crate boundaries

**Recommendation: split `crates/gpu` into a small family of crates with a strict
one-way dependency chain, most of it Vulkan-free and headless-testable.** The
Vulkan-touching code shrinks to a leaf; everything upstream of it (PM4 decode,
state model, shader translation, resource-cache bookkeeping) is pure logic that
runs in the headless devShell.

### Target module tree

```
crates/
  core/            ps4-core — unchanged; still hosts VirtualMemoryManager +
                   KernelInterface. GAINS: the GpuBackend trait + Gpu command
                   channel types (so libs/kernel can reference them without a
                   gpu-crate dep), mirroring how KernelInterface already lives here.

  gnm/             ps4-gnm — NEW. Pure, Vulkan-FREE, headless. The command
                   processor + GPU state model + shader-source abstraction.
    src/
      driver.rs        GnmDriver HLE state (submit-queue bookkeeping, flip
                       labels). Called BY the libSceGnmDriver HLE handlers.
      pm4/
        decode.rs      PM4 Type-3 header walk → Packet enum (opcode+count+body
                       slice). task-21 lives here. Decode-only, no execution.
        opcodes.rs     opcode tables / IT_* constants, register (CONTEXT/SH) enums.
        trace.rs       Packet -> human trace string (UNEMUPS4_PM4_TRACE=1).
      exec.rs          The executor: consumes a decoded stream + the GpuBackend
                       trait, applies state, emits draws/presents/clears. The
                       present/sync, embedded-shader draw, and GCN shader work all
                       grow HERE. Backend-agnostic (takes &mut dyn GpuBackend).
      state.rs         GpuState: the incremental register/binding model (§5).
      shader/
        source.rs      ShaderSource + ShaderProvider trait (§4).
        embedded.rs    embedded-id -> host SPIR-V provider (embedded-shader draw).
        sb.rs          .sb (OrbShdr) header parse (entry to the GCN shader path).
      cache/
        mod.rs         ResourceCache: (guest range, kind, layout) -> host handle,
                       upload-on-use / invalidate-on-dirty (§8). Vulkan-free:
                       talks to the backend's upload/create surface via the trait.

  gcn/             ps4-gcn — the GCN shader path. GCN ISA decode +
                   disassembler + CPU interpreter (the correctness oracle) and
                   the GCN->SPIR-V recompiler. Depends on ps4-core only.
                   Kept OUT of ps4-gnm so the enormous, late shader-translation
                   body never entangles the command processor. exec.rs/shader/sb.rs
                   call into it behind the ShaderProvider trait.

  gpu/             ps4-gpu — the Vulkan/ash BACKEND + presentation, shrunk to a
                   leaf. Implements core::GpuBackend for ash.
    src/
      lib.rs           GpuManager + channel plumbing (as today).
      display.rs       winit loop + swapchain + present + pacing (as today).
      backend/
        ash/…          the ash impl of GpuBackend: device/queue/swapchain,
                       pipeline cache, image/buffer allocation, the present blit,
                       task-18 zero-copy import. (Future: backend/metal/… .)
      present.rs       the softgpu present path (buffer->texture->quad->flip),
                       refactored to sit behind/beside the backend trait.
```

**Dependency direction (strictly one-way, no cycles):**

```
core  ←  gnm  ←  gpu(ash backend)          (libs/kernel → core, as today)
core  ←  gcn  ←  gnm                         (gnm calls gcn behind ShaderProvider)
```

`ps4-gnm` and `ps4-gcn` **never** depend on `ash`, `winit`, or `ps4-gpu`. They
depend on `ps4-core` for `VirtualMemoryManager` (to read guest command
buffers/shader blobs) and for the `GpuBackend`/`ShaderProvider` traits. `ps4-gpu`
depends on `ps4-gnm` to run the executor and implements the backend traits. This
keeps 90%+ of the new code (decode, state, cache bookkeeping, GCN) in
`cargo test`-able crates with no GPU driver.

### Where GnmDriver HLE lives

The **NID handlers** (`sceGnmSubmit*`, `sceGnmDrawIndexAuto`, …) stay in
`crates/libs` as a new `libsceGnmDriver` module, exactly like `libscevideoout` —
they are library-import stubs and belong with the other HLE. But they are *thin*:
a handler extracts the command-buffer pointer/size from guest registers and calls
into `ps4-gnm`'s `GnmDriver`/executor. **The Gnm *logic* lives in `ps4-gnm`; only
the NID-to-Rust glue lives in `libs`.** This matches the videoout split (handler
in libs, framebuffer logic reached via the kernel bridge / GpuManager).

Rationale for a dedicated `gnm` crate rather than a `libs` submodule: PM4 decode +
state + cache is thousands of lines of pure logic with its own unit tests, and it
must be reachable from the `ps4-gpu` backend (the executor drives the backend).
Putting it in `libs` would either drag `libs` into the backend's dependency orbit
or force the executor to live in `gpu` (Vulkan-coupled, un-headless). A separate
Vulkan-free crate is the clean cut.

---

## 2. Rendering-backend abstraction (the load-bearing section)

**Goal:** isolate *what we ask the GPU to do* from *how ash/Vulkan does it*, so a
MoltenVK build is "the same ash backend on a portable subset" and a future native
**Metal** backend is an *alternative impl of one trait*, not a rewrite. The risk is
equal and opposite: a premature, wrong abstraction (a fantasy "GPU HAL" modeled on
no real second backend) is as damaging as raw ash calcifying across the executor.

### The policy: two backends, drawn at the right layer

MoltenVK is a **Vulkan-subset drop-in** — it consumes the same ash calls; the only
delta is *which extensions/features exist*. That delta is already handled by
`decision-3`'s gate-and-fallback rule (task-18's `VK_EXT_external_memory_host` +
staging fallback is the precedent). **MoltenVK therefore needs NO new abstraction
— it needs discipline: every non-portable path gets a capability gate + fallback.**

Native **Metal** is a genuinely different API. That is the only thing that
justifies a backend trait. So the trait is drawn to be *the seam a Metal impl would
need*, and nothing finer.

### What to introduce NOW (before task-20 lands): `GpuBackend` in `ps4-core`

Introduce a **narrow** trait that captures the operations the *executor* performs,
at the granularity the executor thinks in (PS4/PM4 concepts), not raw Vulkan verbs.
The current present path is re-expressed through it; the PM4 executor is written
against it from day one so no raw `ash::vk` type ever reaches `ps4-gnm`.

```rust
// ps4-core::gpu (sketch — NOT final)

/// Opaque, backend-owned handles. The executor/cache hold these, never vk::*.
pub struct PipelineId(pub u32);
pub struct TargetId(pub u32);   // a color/render target (incl. the videoout fb)
pub struct ResourceId(pub u32); // a cached buffer/texture (see §8)

pub enum Prim { TriList, TriStrip, /* … */ }

/// The whole GPU surface the command-processor drives. One impl per API.
/// Deliberately coarse: "present a target", "draw with this pipeline+bindings",
/// not "cmd_pipeline_barrier". Metal can satisfy all of these.
pub trait GpuBackend: Send {
    // ---- presentation (the present-only path, exists today, just relocated) ----
    /// Present the given host target to the display (softgpu framebuffer today).
    fn present(&mut self, target: TargetId) -> Result<(), GpuError>;

    // ---- resource cache backing (embedded-shader draw onward, see §8) ----
    fn create_target(&mut self, desc: &TargetDesc) -> TargetId;
    fn create_resource(&mut self, desc: &ResourceDesc) -> ResourceId;
    /// Upload host bytes into a cached resource (copy+track path; §8).
    fn upload(&mut self, id: ResourceId, offset: u64, bytes: &[u8]);
    /// Optional zero-copy import of an identity-mapped guest range (task-18 path);
    /// returns None on backends/ranges that can't (MoltenVK, unaligned) -> caller
    /// falls back to create_resource+upload. This makes zero-copy vs copy ONE seam.
    fn try_import_host_range(&mut self, host_ptr: *const u8, size: u64)
        -> Option<ResourceId>;

    // ---- pipeline + draw (embedded-shader draw onward) ----
    /// Build (or fetch from cache) a pipeline from a resolved shader pair + state.
    fn get_or_create_pipeline(&mut self, key: &PipelineKey) -> PipelineId;
    fn begin_target(&mut self, target: TargetId, clear: Option<[f32; 4]>);
    fn bind_pipeline(&mut self, p: PipelineId);
    fn bind_resource(&mut self, slot: BindSlot, r: ResourceId);
    fn draw(&mut self, prim: Prim, vertex_count: u32, first: u32);
    fn end_target(&mut self);

    // ---- sync (PM4 present/sync) ----
    fn signal_eop(&mut self, label_addr: u64, value: u64); // EOP -> equeue bridge
}
```

Notes on the surface:

- **`present` + the softgpu path exist today** and are the *only* methods the
  present-only path needs — so the very first refactor is "make the current display
  loop call `backend.present()`", nothing more. Everything else is a stub until its
  increment lands.
- The trait speaks in **`PipelineKey`/`ResourceId`/`TargetId`**, never `vk::*`.
  `record_command_buffer`, barriers, render passes, descriptor sets — all stay
  *inside* the ash backend. `ps4-gnm` cannot even name a Vulkan type.
- **`try_import_host_range` returning `Option`** folds task-18's zero-copy into the
  same trait as the copy path (see §8): the executor/cache asks the backend to
  import; a `None` means "fall back to `create_resource`+`upload`". On MoltenVK it
  is always `None`. This is the "one policy, two mechanisms" cut the coordinator
  asked for.

### What to DEFER (do not build until a second backend exists)

- **No command-buffer/encoder abstraction.** Do not invent a portable
  "CommandEncoder" wrapping barriers/passes. That is where fantasy-HAL projects
  drown. The backend records its own native command buffer internally between
  `begin_target` and `end_target`.
- **No render-graph, no descriptor-abstraction, no memory-allocator trait.** ash's
  `gpu-allocator` stays a backend detail.
- **No Metal scaffolding, no `#[cfg]` backend switch, no dyn-dispatch registry**
  until MoltenVK is actually running and a Metal port is actually scheduled. Ship
  the trait with exactly one impl (`ash`). A trait with one impl is cheap
  insurance; a second impl written speculatively is waste.
- **Keep the trait small and grow it per increment.** Add `draw`/`bind_*` when the
  embedded-shader draw work needs them, not now. The *concept* (a trait exists,
  `ps4-gnm` targets it) is what must land early; the *method list* accretes with the
  increments.

**Bottom line:** introduce the `GpuBackend` trait now, in `ps4-core`, with only the
present/import surface implemented and the rest sketched; write the executor
against it; keep ash as the sole impl. That single move prevents raw ash from
spreading into the PM4 code — which is the actual calcification we are avoiding.

---

## 3. Command-processor design

**Flow:** `libSceGnmDriver` submit HLE (in `libs`) → `GnmDriver` (in `ps4-gnm`)
hands the guest command-buffer range to → **PM4 decoder** (`pm4/decode.rs`,
task-21) producing a `Packet` stream → **executor** (`exec.rs`) which maps packets
onto `GpuBackend`. The seam is built so **trace-only, present-subset, and full-draw
modes are three configurations of one pipeline, not forks.**

### One pipeline, mode-gated packet handlers

```rust
// ps4-gnm::exec (sketch)

pub struct Executor<'a> {
    state: GpuState,                 // §5: grows per increment
    backend: &'a mut dyn GpuBackend, // None-equivalent in trace-only mode
    cache: &'a mut ResourceCache,    // §8
    mode: ExecMode,
}

pub enum ExecMode { TraceOnly, PresentSubset, Draw }

impl Executor<'_> {
    pub fn run(&mut self, mem: &dyn VirtualMemoryManager, cb: GuestRange) {
        for pkt in pm4::decode(mem, cb) {           // decode is mode-independent
            if trace_enabled() { log(pm4::trace(&pkt)); }  // trace, always on w/ env
            match pkt.op {
                Op::SetContextReg(..) | Op::SetShReg(..) => self.state.apply(&pkt),
                Op::Nop | Op::Unknown(_) => { /* log, never fatal (AC #3) */ }
                Op::EventWriteEop(..) if self.mode >= PresentSubset
                    => self.backend.signal_eop(..),          // present/sync
                Op::DrawIndexAuto(..) if self.mode == Draw
                    => self.emit_draw(&pkt),                  // embedded-shader draw onward
                _ => { /* decoded + traced, execution deferred to its increment */ }
            }
        }
    }
}
```

The key property: **decode and trace run in every mode.** `ExecMode::TraceOnly`
touches no backend at all — fully headless. The present/sync mode flips present/sync
opcodes on. The draw mode flips `Draw` on. Each mode *adds match arms*; none rewrites
the loop, and each earlier mode's arms keep working. Unknown opcodes are logged and
skipped in all modes (task-21 AC #3).

### The thread boundary (concrete)

Submits arrive on the **guest thread** (inside the GnmDriver HLE handler). Vulkan
lives on the **display thread**. Two options, and the recommendation:

- **Recommended:** the decoder+state+cache-bookkeeping (all Vulkan-free) run **on
  the guest thread** inside the handler; the executor emits **backend commands as
  a data list** (an enum of `present/create/upload/draw/…`) that is **sent over the
  existing crossbeam channel** to the display thread, which replays them against
  the ash backend. This keeps every Vulkan call on the one thread that owns the
  device (no cross-thread device sharing), and keeps the headless-testable logic
  off the display thread entirely. `SubmitAndFlip` reuses the current
  block-until-vsync handshake. The `GpuCommand` enum simply grows a
  `RunCommandList(Vec<BackendCmd>)` variant beside `RegisterBuffer`/`SubmitFlip`.
- **Alternative (rejected for now):** move the device onto its own GPU thread and
  have the executor call the backend directly. A dedicated async GPU
  thread, but it means sharing/relocating the Vulkan device and rewriting the
  present handshake. Defer until perf demands it; the channel approach preserves
  today's working 60fps present path untouched.

This is why `GpuBackend` methods are *record-like* (`begin_target`/`draw`/…): they
map cleanly onto a serializable `BackendCmd` list crossing the channel.

**Submission is multi-stream / multi-ring from the start (see §C1, §C6, §C2).** The
executor's entry point is not "one buffer" but "a set of typed streams on a ring":
`run(ring, &[(StreamKind::{Dcb,Ccb,Acb}, GuestRange)])`. The decode+execute path runs
the DCB only, on the graphics ring, but the DCB+CCB pairing, ACB parallelism, and a
ring/queue id are carried through so async compute (C1) and CE/DE coupling (C6) are
added data, not a rewrite. GPU completion is a **timeline** (C2): `signal_eop` is one
op of an EOP/EOS/wait-label vocabulary mapped to Vulkan timeline semaphores.

---

## 4. Shader abstraction

**Seam:** a `ShaderProvider` that turns a *shader reference seen in the PM4 stream*
into a *host pipeline shader*, with two interchangeable providers behind it, so the
embedded-shader path does not bake in assumptions the GCN shader path must tear out.

```rust
// ps4-gnm::shader::source (sketch)

/// How a shader entered the PM4 stream.
pub enum ShaderRef {
    Embedded { stage: Stage, id: u32 },   // sceGnmSetEmbeddedVs/PsShader(id)
    GcnBinary { addr: u64, /* OrbShdr .sb in guest memory */ },
}

/// Backend-agnostic resolved shader: SPIR-V today; a Metal provider could return
/// MSL/lib. The executor never sees GCN or SPIR-V bytes directly.
pub struct HostShader { pub stage: Stage, pub spirv: Arc<[u32]>, /* + io layout */ }

pub trait ShaderProvider {
    /// Ok(Some) = resolved; Ok(None) = "not my kind" (chain to next provider);
    /// Err = recognized-but-unsupported (clean defer, e.g. a shader feature not yet handled, AC #3).
    fn resolve(&self, r: &ShaderRef, mem: &dyn VirtualMemoryManager)
        -> Result<Option<HostShader>, ShaderUnsupported>;
}
```

- **`EmbeddedShaderProvider`:** recognizes `Embedded{VS,0}` (fullscreen
  quad) and `Embedded{PS,1}` (R/G export) and returns **hardcoded host SPIR-V**
  (the fixed set from doc-1). No GCN. A draw bound to a `GcnBinary` isn't its kind,
  so it returns `Ok(None)` and the chain falls through to `GcnShaderProvider` (below).
- **`GcnShaderProvider`** (in/over `ps4-gcn`): parses the `OrbShdr`
  `ShaderBinaryInfo` header (magic `"OrbShdr"`, `m_type`, `m_length`, semantic
  tables), decodes GCN, and produces SPIR-V — via the **CPU interpreter as
  oracle** and the recompiler, differential-tested against each other
  (`decision-3`). Same `HostShader` output; the executor is unchanged.
- Providers are **chained** (`resolve → None` means "try the next"), so the
  embedded provider stays live and takes precedence for embedded IDs alongside the
  GCN provider. The resolved `HostShader` feeds `PipelineKey` →
  `backend.get_or_create_pipeline` (the backend caches pipelines by key).

**What the embedded path must NOT hardcode** (traps for the GCN path): the executor
must route *all* shader binds through `ShaderRef`/`ShaderProvider` even for embedded
shaders — it must not special-case "there are only two shaders" anywhere except
inside `EmbeddedShaderProvider`. `PipelineKey` must already carry a shader *identity*
(hash/addr), vertex-input layout, and RT format, not a hardcoded pipeline handle —
otherwise the GCN path's arbitrary shaders have nowhere to key on. Two further seams
the shader path must leave open (details in §C4, §C8): `HostShader`/`ShaderRef` carry
a **HW-stage role** (LS/HS/ES/GS/VS ≠ logical stage, C8) rather than assuming
logical==hardware, and the binding model is **descriptor-from-memory** (V#/T#/S#
resolved from user-SGPR-pointed memory, C4), not Vulkan-descriptor-set shaped — even
though the embedded shaders are a fixed HW-VS+PS pair with fixed bindings.

---

## 5. State model

PS4 GPU state (context registers, SH registers, render targets, PSSL resource
bindings) is large. **Do not model it as one giant struct that every increment rewrites.**
Instead: a `GpuState` that is (a) a sparse register file plus (b) a small set of
*derived, typed* views that grow per increment.

```rust
// ps4-gnm::state (sketch)
pub struct GpuState {
    ctx_regs: RegFile,   // CONTEXT_REG writes (sparse: index -> u32)
    sh_regs:  RegFile,   // SH_REG writes (shader addrs, user data)
    // typed views, added as increments need them:
    targets: RenderTargets,   // embedded-shader draw: color RT = the videoout fb at first
    vtx:     VertexBindings,  // embedded-shader draw
    shaders: BoundShaders,    // embedded-shader draw: current VS/PS as ShaderRef
    // the GCN shader path adds: textures, samplers, more RTs, depth, blend, …
}
```

- **`SetContextReg`/`SetShReg` packets just write into the sparse `RegFile`** — that
  handler is written *once* (the trace path can even trace it) and never changes. New
  state is "understanding more register indices", not "new packet handling".
- **Derived views are lazy and additive.** The embedded-shader draw adds
  `targets`/`vtx`/`shaders` by *interpreting* the relevant register ranges at draw
  time. The GCN shader path adds `textures` etc. the same way. Adding a view never
  disturbs existing ones.
- **A draw reads state, snapshots the bits the pipeline depends on into a
  `PipelineKey`, and hands it to the backend.** So the state model and the backend
  never share mutable structure; the key is the contract.

This mirrors how real hardware works (registers are the truth; "pipeline state" is
derived) and means the GCN path's explosion of state is *addition*, not *migration*.
Concretely (§C7): the `RegFile`s are the **shadow register file** — three banks
`SET_CONTEXT_REG` / `SET_SH_REG` / `SET_UCONFIG_REG` — and the derived views are the
**register→pipeline-state translation applied at draw time**. New state = decoding
more register indices, never restructuring.

---

## 6. Testability

The architecture is built so the **decode/state/cache/GCN logic is separable from
the driver-touching backend** and runs in the headless devShell (no Vulkan). This
is a hard constraint: the devShell has no Vulkan driver (task-18's blocker), so CI-
style tests must not need one.

- **PM4 decoder (task-21):** pure `mem: &dyn VirtualMemoryManager → Vec<Packet>`.
  Unit-tested with a **mock memory** (a `Vec<u8>` behind the trait) holding hand-
  crafted command buffers; assert opcode/count/trace output. Zero GPU. This is why
  `pm4/decode.rs` takes the `ps4-core` memory trait, not any GPU handle.
- **Executor & state:** run the executor with a **`MockBackend`** (an in-memory impl
  of `GpuBackend` that records `BackendCmd`s into a `Vec`). Assert "this PM4 stream
  produced this sequence of create/upload/bind/draw/present calls". Fully headless.
  Because the executor only ever calls the trait, the mock is trivial.
- **Resource cache (§8):** unit-tested against `MockBackend` — write a range, use
  it (assert one upload), mark it dirty, use again (assert re-upload), leave clean
  (assert no re-upload). No GPU.
- **GCN interpreter as differential oracle:** the interpreter and the
  recompiler both produce results for the same guest shader over the same inputs;
  tests compare them (`decision-3`). The interpreter needs no GPU; the recompiler's
  SPIR-V can be validated with `spirv-val` offline and, where a driver exists,
  executed by the maintainer.
- **Backend impl (`ps4-gpu` ash):** the *only* part that needs a real device.
  Verified by the maintainer running example ELFs (task-22 corpus), same as task-18.
  Kept as thin as possible precisely so little logic hides behind the untestable line.

The `MockBackend` + mock-memory pair is the linchpin: they let the
decode/state/cache/GCN logic be `cargo test`-covered while the real GPU work stays a
small, manually-verified leaf.

---

## 7. Migration from today (order of refactors)

The emulator must keep presenting softgpu at 60fps throughout. Order, each step
independently shippable and leaving present working:

1. **[PREREQUISITE — before task-20 code] Extract `GpuBackend` (present-only) into
   `ps4-core`; make the display loop drive it.** Introduce the trait with just the
   present/import surface (§2). Move the softgpu present path (`record_command_buffer`
   blit, task-18 zero-copy) behind an `AshBackend` impl. `run_display_loop` calls
   `backend.present(target)` instead of open-coding the chain. **No behavior change**
   — same blit, same fps, same zero-copy/staging fallback. This is the one refactor
   that must precede task-20 (see recommendation below).
2. **Create the empty `ps4-gnm` crate** (Vulkan-free) with `pm4/`, `state.rs`,
   `exec.rs` skeletons and the `ShaderProvider`/`ResourceCache` trait stubs. No
   wiring yet. Pure scaffolding; compiles, does nothing.
3. **task-20:** add the `libsceGnmDriver` HLE module in `libs`; handlers call a
   `GnmDriver` stub in `ps4-gnm` that just records submit ranges. Guest boots.
4. **task-21:** implement `pm4/decode.rs` + `trace.rs` + `ExecMode::TraceOnly`.
   Unit-tested headless. task-22 produces a trace.
5. **Present/sync:** grow the executor's present/sync arms; add `RunCommandList` to
   the channel; `SubmitAndFlip` → existing present path; EOP → equeue. Present path
   from step 1 is reused, not replaced.
6. **Embedded-shader draw (task-24):** add draw arms, `EmbeddedShaderProvider`, the
   minimal `ResourceCache` (vertex/index/constant buffers), `get_or_create_pipeline`.
   First GPU-drawn frame. Softgpu present still works for software-framebuffer guests.
7. **GCN shader path:** create `ps4-gcn`, add `GcnShaderProvider`, grow cache to
   textures/RTs, interpreter-then-recompiler.

Throughout, the present path from step 1 is the stable floor: any guest that only
uses videoout keeps working exactly as today.

---

## 8. Unified memory → host resource cache + invalidation (first-class seam)

The PS4 has **unified memory** (GDDR5 shared CPU/GPU): the guest treats every GPU
resource — textures, vertex/index buffers, constant buffers, shader `.sb` binaries,
render targets — as plain memory it writes to. The host has **separate VRAM**, so
for every guest memory range the GPU references we must decide *how it reaches the
GPU* and *how we notice when the guest changes it*. Getting this wrong is what makes
the subsystem collapse at scale (it is emulation's hardest subsystem). It is
designed here as **one policy — "how does this guest range reach the GPU" — with
zero-copy and copy+invalidate as two points on it**, not as unrelated mechanisms.

### 8.1 The cache abstraction

Lives in `ps4-gnm::cache` (Vulkan-free; drives the backend via the trait):

```rust
// ps4-gnm::cache (sketch)
#[derive(Hash, Eq, PartialEq)]
pub struct ResourceKey { pub addr: u64, pub size: u64, pub layout: ResLayout }
// ResLayout = kind (Texture{format,tiling}, VertexBuf, IndexBuf, ConstBuf, ShaderBin,
//             RenderTarget{format}) + whatever else disambiguates the SAME bytes
//             viewed two ways (a range aliased as both RT and texture -> two keys).

pub struct ResourceCache { /* map<ResourceKey, Entry> ; Entry{ id: ResourceId, policy, epoch } */ }

impl ResourceCache {
    /// The single entry point: "I need this guest range on the GPU as `layout`."
    /// Chooses policy, imports or uploads, returns the backend handle.
    pub fn get(&mut self, key: ResourceKey, mem: &dyn VirtualMemoryManager,
               backend: &mut dyn GpuBackend, dirty: &dyn DirtySource) -> ResourceId;
    pub fn invalidate_range(&mut self, addr: u64, size: u64); // guest wrote here
}
```

`get` on first use: pick policy (below), create the host resource, upload (or
import), record the key. On subsequent use: if the range is **clean**, return the
cached `ResourceId` with no work; if **dirty**, re-upload (copy path) then clear the
flag. Keyed by `(addr, size, layout)` so the same bytes seen as two resource kinds
get two entries (the RT-as-texture aliasing case, §8.5).

### 8.2 Policy: one spectrum from zero-copy to copy+invalidate

```
guest range needed on GPU
        │
        ├─ backend.try_import_host_range() -> Some(id)   [ZERO-COPY end]
        │     GPU reads guest pages directly (task-18, VK_EXT_external_memory_host).
        │     No upload, NO invalidation needed — the GPU always sees current bytes.
        │     Available: read-mostly, large, aligned ranges on Vulkan desktop.
        │     NOT available on MoltenVK -> falls through.
        │
        └─ create_resource + upload + DIRTY-TRACK           [COPY end, PORTABLE DEFAULT]
              Host VRAM copy; must re-upload when the guest writes the backing pages.
              This is the default everywhere MoltenVK runs, and for tiled textures
              (host layout != guest layout, so zero-copy is impossible regardless).
```

`try_import_host_range` returning `Option` is exactly this fork (§2). **MoltenVK
lacks `external_memory_host`, so copy+invalidate is the portable default and
zero-copy is a desktop optimization** — never the correctness-critical path.
Render targets and tiled textures are always copy-side (their host layout differs
from the guest's linear bytes), so they always need invalidation/readback.

### 8.3 Invalidation via dirty tracking — and the x86jit reality

Re-upload must be driven by "the guest wrote a page backing a cached resource".
Options, with the concrete finding on x86jit:

- **Reuse x86jit SMC dirty tracking — investigated, NOT directly usable today.**
  x86jit *does* track dirty pages, but **only for pages explicitly tagged as
  code** via `mark_code` (`x86jit-core/src/memory.rs`): `note_write` records a
  dirtied page **only if that page was previously `mark_code`'d**, and
  `take_dirty_code()` drains that code-page set. A texture/vertex-buffer page the
  guest writes is a *non-code* page — `note_write` does one relaxed load and
  returns, recording nothing. So `take_dirty_code` **cannot** serve as the GPU
  cache's dirty source as-is. What's needed is a *parallel* facility: register a
  set of *watched data ranges* and drain the ones written since last poll —
  independent of the code-page mechanism, though it can share the same `note_write`
  hot-path check. **This is an x86jit capability we do not have and must request
  via x86jit's own backlog (never edit x86jit directly).** See open questions.
  (Also note `MemConsistency::Fast` is set — the dirty facility must not depend on
  ordering guarantees Fast doesn't provide; a poll-and-drain model at frame/submit
  boundaries fits.)
- **Guest-write hooks in the HLE path (available now, coarse).** Guest GPU memory
  is written either through direct guest stores (invisible to us without x86jit
  help) or via memory the guest allocated and we can bracket. We can conservatively
  invalidate on known events: at each `SubmitAndFlip`/`Submit`, treat the referenced
  vertex/const/index ranges as dirty (re-upload every submit). Correct but wasteful
  — this is the "upload everything every frame" approach and is the acceptable
  **starting point for the embedded-shader draw** because the corpus is tiny. It
  needs no x86jit change.
- **`mprotect`-based dirty pages (host-side, driver-free).** Mark cached ranges
  read-only in the host page tables, catch `SIGSEGV` on guest write, mark dirty,
  restore. Precise and independent of x86jit — but it **fights the identity-mapped
  arena and x86jit's own memory model** (x86jit may itself manage protections /
  SMC), risks signal-handler/JIT interaction bugs, and MoltenVK/macOS signal
  semantics differ. Higher-risk; keep as a fallback investigation, not the plan.

**Recommended sequence:** the embedded-shader draw uses **submit-time conservative
invalidation** (re-upload referenced buffers each submit; corpus is one draw, cost is
nil). Before the GCN texture cache makes that too expensive, land a
**watched-data-range dirty API in x86jit** and switch the cache to poll it. `mprotect`
stays a documented fallback if the x86jit API doesn't materialize.

```rust
// ps4-core (sketch) — the seam the cache polls; impls: x86jit-backed, or
// "everything dirty" (embedded-shader draw), or mprotect-backed (fallback).
pub trait DirtySource {
    fn watch(&self, addr: u64, size: u64);
    fn take_dirty(&self) -> Vec<(u64, u64)>; // ranges written since last drain
}
```

### 8.4 Where it lives / how it threads in

- **Module:** `ps4-gnm::cache` (Vulkan-free logic). It calls
  `GpuBackend::{create_resource, upload, try_import_host_range}` — so the **backend
  trait gains exactly that upload/create/import surface** (already in the §2 sketch)
  and nothing more; VRAM allocation stays inside the ash backend.
- **DirtySource** is a `ps4-core` trait (like `KernelInterface`), impl'd against
  x86jit (once the API exists) and wired at runtime via the same `OnceLock` pattern,
  or by a trivial "always dirty" impl for the embedded-shader draw.
- **Testability:** the cache is unit-tested headless with `MockBackend` +
  a mock `DirtySource` (a `Vec` of ranges you flip): use→assert-one-upload,
  dirty→assert-reupload, clean→assert-none (§6).

### 8.5 Render targets: asymmetric sync (upload always on, readback gated OFF)

RTs are the subtle case — the **GPU writes them**, and *some* guests **read them
back** (CPU postprocess) or the RT is **sampled as a texture** by a later draw. The
two directions have **deliberately asymmetric defaults**, because they have wildly
different cost:

- **guest→GPU (upload): always on**, driven by dirty tracking (§8.3). Moving data
  GPU-ward lazily is the entire point of the cache and is cheap in aggregate.
- **GPU→guest (readback): gated OFF by default.** Copying a GPU-written RT back into
  its guest range forces a **GPU stall + a full-target transfer every frame** — one
  of emulation's worst perf killers. Many titles never read their RTs on the CPU, so
  paying it universally is wrong. **Readback is the expensive reverse direction and
  must never sit on the default hot path.**

**RT-as-texture aliasing is HOST-ONLY and does NOT need readback.** When a later
draw samples a range a prior draw rendered to, the same guest range appears under
two `ResourceKey`s (`RenderTarget{fmt,tiling}` and `Texture{fmt,tiling}`). The cache
detects the overlap and resolves it **entirely host-side** — a GPU blit/alias from
the host RT into the sampled host resource via the backend — with **no copy to guest
memory**. This is pure host resource aliasing in the cache and is independent of the
CPU-visible readback path below. This is part of the GCN path; the embedded-shader
draw's only RT is the videoout framebuffer, already handled by the present path.

**Readback (opt-in, correctness-over-speed fallback).** Only when readback is enabled
does a GPU-written RT get copied back to its guest range (and its cache entry marked
clean) so the guest CPU sees current pixels. Policy surface:

```rust
// ps4-gnm::cache (sketch) — readback policy, DEFAULT Off.
pub enum ReadbackPolicy { Off, All /* future: PerTitle(set-of-RT-ranges) */ }
// resolved once from an env lever now (e.g. UNEMUPS4_RT_READBACK=1), mirroring the
// task-18 UNEMUPS4_NO_EXTMEMHOST / profile env levers; per-title override later.
```

The backend gains a `readback(target) -> bytes` method **only when the readback path
is built** (part of the GCN path, and only exercised when the policy is on). On the fast default
path the RT lives **host-side only**; its backing guest memory is intentionally **not
kept in sync**, and that is correct for the games that never read it. This is why RTs
are keyed separately and (being tiled/host-layout) are never zero-copy-imported.

### 8.6 Sequencing of the cache

| Increment | Cache scope | Invalidation |
|---|---|---|
| Present/sync | none new — framebuffer only, present path already handles it (zero-copy import or staging, task-18) | n/a |
| Embedded-shader draw | vertex / index / constant buffers (small, linear) | conservative: re-upload referenced ranges each submit |
| GCN shader path | + textures (**detiled on upload**, §C3), + shader `.sb` binaries, + multiple RTs, host-side RT-as-texture aliasing, opt-in readback | x86jit watched-range DirtySource (or mprotect fallback); host RT resolve always, guest readback only if `ReadbackPolicy` on |

---

## 9. PS4 architecture constraints to design for (implement later)

Five structural facts of the real hardware are cheap to leave room for now and a
rewrite to retrofit. **This section does NOT expand scope** — it records the
structural facts to leave room for, not a mandate to build them all up front (some,
like tiled-texture detile and V#/T#/S# descriptor decode, have since landed; others
remain deferred). Each entry states the *minimal seam to leave now* vs. what to
*genuinely defer*, and cross-references the section it touches. The discipline: don't bake in an
assumption that precludes them; don't build them speculatively either.

### C1. Multi-queue / async compute (ACE) — submission model

GCN exposes a graphics ring **plus 8 ACE compute engines × 8 queues**; games run
compute concurrent with graphics (`sceGnmMapComputeQueue`/`sceGnmDingDong` are the
async-compute NIDs stubbed in task-20). **Trap:** a single-serial-ring assumption
baked into the executor.

- **Seam now (§3):** model submission as **multi-ring/multi-queue from the start** —
  `run(cb)` is really "run this command buffer on *ring R*"; the `GpuBackend` maps a
  ring to a Vulkan queue. Implement **only the graphics ring**, but let the executor
  and `BackendCmd` list carry a ring/queue id so a second ring is added data, not a
  rewrite. `sceGnmSubmitCommandBuffers` already takes DCB+CCB pairs — thread the
  buffer-*kind* through even while only DCB is executed.
- **Defer:** actual ACE execution, DCB/CCB/ACB interleaving, per-queue scheduling.
- **Portability:** MoltenVK exposes **few Vulkan queues** (often 1–2); map multiple
  PS4 rings onto available queues with graceful degradation to serialized execution
  on one queue. Flagged in open questions.

### C2. GPU sync primitives (EOP / EOS / labels / fences) — timeline model

The GPU writes a value to memory on completion; the CPU or another ring waits on it.
This is the backbone of GPU↔CPU and GPU↔GPU ordering. The present/sync work already
routes EOP → equeues (§3, mapping table) — **that must be the thin edge of a coherent sync model,
not a one-off.**

- **Seam now:** treat GPU completion as a **timeline** — `GpuBackend::signal_eop(label_addr,
  value)` (already in the §2 sketch) is one operation of a small sync vocabulary
  (EOP, EOS, wait-on-label). Map to **Vulkan timeline semaphores**; a written label
  in guest memory is the CPU-visible mirror. Design the executor so "GPU writes label
  X = V, ring/CPU waits X >= V" is a first-class concept the EOP arm is an instance of.
- **Defer:** GPU↔GPU cross-ring waits (needs C1), the full label/EOS taxonomy.
- **Portability:** MoltenVK's timeline-semaphore support is **variable** — gate it,
  fall back to fences/host-visible label polling per `decision-3`. Open question.

### C3. Tiled / swizzled texture & RT layouts — cache key + detile step

PS4 textures and render targets are stored **tiled (swizzled)**, not linear.
**Detiling on upload (and re-tiling on readback) is mandatory** — if the cache
assumes linear bytes, every texture is corrupt.

- **Seam now (§8):** the `ResourceKey`'s `ResLayout` **already carries a tiling
  field** (`Texture{format,tiling}`, `RenderTarget{format,tiling}`) — keep it there
  from day one, and make the upload path go through a **`detile(bytes, layout) ->
  linear`** step (identity for linear layouts). The embedded-shader draw's buffers are
  linear so the step is a no-op there, but the seam exists.
- **Defer:** the actual tiling/detiling math (GCN micro/macro-tiling, per-format
  swizzle — reference `freegnm`/AddrLib), and re-tile-on-readback.
- Zero-copy import is impossible for tiled resources (host layout ≠ guest bytes), so
  tiled textures/RTs are always copy+detile side of the §8.2 spectrum.

### C4. GCN descriptor / binding model (V# / T# / S#, fetch shaders) — shader seam

GCN shaders don't use Vulkan-style descriptor sets: they **load resource descriptors
from memory** (V# buffer / T# texture / S# sampler) via addresses in user-SGPRs,
often through a **fetch shader** for vertex attributes. **Trap:** the embedded
shaders have fixed bindings; if the binding model bakes that in, the GCN path tears it out.

- **Seam now (§4, §5):** shape the binding abstraction as **descriptor-from-memory**.
  `PipelineKey`/`GpuState` express bindings as "resource descriptors resolved from a
  guest memory region (pointed to by user-SGPRs)", and the `ShaderProvider`/`HostShader`
  I/O layout carries the semantic tables (V#/T#/S# slots, vertex-input semantics from
  the `.sb` header). The `EmbeddedShaderProvider` returns a *fixed* such layout
  — but the executor consumes it through the same memory-driven path, so the GCN path's
  real descriptors slot in without changing the executor.
- **Defer:** fetch-shader emulation, actual V#/T#/S# decode, SRT (shader resource
  tables) — all inside `ps4-gcn`.

### C5. Onion vs garlic memory — memory-type flag threaded into cache policy

Unified memory has two coherence views: **onion** (CPU-coherent, cached) and **garlic**
(GPU-optimized, CPU-uncached, write-combined). Games map GPU resources as one or the
other, and it **directly feeds the §8.2 cache policy**: garlic = GPU-read-direct =
**zero-copy candidate** (task-18 style); onion = CPU-touched = **copy + dirty-track**.

- **Seam now (§8):** the resource cache's per-range policy decision must be able to
  read a **memory-type flag** for the range. That flag originates in the **kernel
  memory manager** (`sceKernelAllocateDirectMemory` / map with cache attributes) and
  must be **threaded from `ps4-memory`/the kernel into the cache** — a memory-manager
  ↔ GPU-cache dependency. Leave the `ResourceCache::get` policy step able to consult
  it (even if the current cache always treats everything as copy-side); don't hardcode a
  single coherence assumption.
- **Defer:** honoring write-combine semantics precisely, onion/garlic-specific
  fast paths. Note: the flag is a *hint that selects §8.2 policy*, not new mechanism.

### C6. Multi-stream command model: DCB + CCB (+ ACB) with CE/DE counter sync

A Gnm graphics submission is **not one PM4 stream**. It is a **pair**: a **DCB**
(Draw Command Buffer, run by the Draw Engine / DE) + a **CCB** (Constant Command
Buffer, run by the Constant Engine / CE), plus separate **ACB** streams for async
compute. The **CE runs ahead of the DE**, preloading constant data
(`WriteConstRam`/`DumpConstRam` into a constant heap), and the two synchronize via
counters (`IncrementCeCounter` / `WaitOnDeCounterDiff`); the DE and CE are
processed concurrently. `sceGnmSubmitCommandBuffers` already takes
DCB **and** CCB address/size arrays. **Trap:** a single-linear-buffer decoder.

- **Seam now (§3):** the executor's **submission abstraction takes a *set* of
  coordinated streams** (typed DCB/CCB/ACB), not a single buffer, with an
  inter-stream **counter/sync** hook. task-21's Type-3 decode can still start on one
  stream, and execution can run DCB only — but `GnmDriver`/`Executor` must
  carry the DCB+CCB pairing and leave room for ACB parallelism (ties into C1's
  ring/queue id). Concretely: `run` receives `&[(StreamKind, GuestRange)]`, not one
  range.
- **Defer:** CE-ahead-of-DE concurrency, the constant-heap (`WriteConstRam`) model,
  actual counter synchronization, ACB execution.
- Sources: AMD GCN PM4 command-processing docs (DE/CE engines, constant heap); the
  real PS4 console DCB/CCB capture (celeste-scrape-oracle).

### C7. GPU state = a shadow register file (concrete answer to §5)

PS4 GPU state is set almost entirely via PM4 register writes: **`SET_CONTEXT_REG`,
`SET_SH_REG`, `SET_UCONFIG_REG`** (context / shader / uconfig banks). The executor
maintains a **shadow register file** and **derives host pipeline state from register
values at draw time** — a "register → pipeline state" translation — rather than
modeling state as ad-hoc fields.

- **This is the concrete form of §5.** The `GpuState` `ctx_regs`/`sh_regs` `RegFile`s
  become three banks (context/SH/uconfig); the `SetContextReg`/`SetShReg` handlers
  (and a new `SetUConfigReg`) just write indices; the derived views in §5 are exactly
  the "derive pipeline at draw" step. **Design it this way now** so new state means
  *decoding more register indices*, never restructuring.
- **Defer:** the exhaustive register→state mapping (hundreds of registers); grow it
  as draws need each field.
- Sources: AMD GCN PM4 register banks (SET_CONTEXT_REG / SET_SH_REG / SET_UCONFIG_REG); Mesa src/amd.

### C8. Shader hardware-stage remapping (LS/HS/ES/GS/VS ≠ logical stages)

GCN hardware vertex/geometry stages (**LS, HS, ES, GS, VS**) do **not** map 1:1 to
logical/API stages. The driver picks which HW stages a pipeline uses, and the **last
logical stage before rasterization must compile to HW-VS** (the rasterizer only
consumes HW-VS output). A logical vertex shader compiles as **HW-LS** (with
tessellation), **HW-ES** (with geometry), or **HW-VS** (neither); GS is the awkward
case; Vega **merges LS+HS and ES+GS**. **Trap:** the embedded shaders are a
simple HW-VS+PS pair; baking `logical==hardware` in precludes the GCN path.

- **Seam now (§4, §7):** `HostShader`/`ShaderRef` and the pipeline derivation carry a
  **HW-stage role** (derived from register state), not a logical stage. The
  `ShaderProvider` returns "this is HW-VS / HW-LS / …"; the executor keys pipelines on
  HW-stage roles. `EmbeddedShaderProvider` returns HW-VS+PS — through the same
  role-carrying seam.
- **Defer:** tessellation/geometry stage handling, LS+HS / ES+GS merging —
  inside `ps4-gcn`.
- Sources: AMD GCN stage remapping (Timur Kristóf's NGG writeups; mesa/radv notes).

### C9. Compressed surfaces: DCC (color) + HTILE (depth)

Render targets use **Delta Color Compression (DCC)** and depth buffers use **HTILE**,
both with **separate metadata surfaces in guest memory**. Sampling a compressed RT as
a texture, or reading it back, requires **decompression** (or a decompress-in-place
pass). **Trap:** a cache surface model that assumes uncompressed.

- **Seam now (§8):** the `ResLayout` in `ResourceKey` **carries compression state +
  metadata pointer(s)** alongside format/tiling, even though the first implementation
  **forces surfaces uncompressed** (a stated correctness-first, perf-later choice).
  The §8.5 host RT-as-texture resolve and the opt-in readback path both consult this
  field so a compressed surface can later gain a decompress step without reshaping the
  key.
- **Defer:** DCC/HTILE decode, decompress passes, compressed-sample fast paths.
- Sources: AMD GCN DCC/HTILE docs; Mesa src/amd surface handling.

**Honest framing:** for C1/C2/C6/C7 the seam is "carry a ring id / a timeline op / a
stream set / a register bank" — nearly free and worth doing now. For C3/C4/C5/C8/C9
the seam is "the key/layout/binding/policy/stage field already exists" — also cheap.
Everything behind those fields (tiling math, GCN descriptor decode, ACE scheduling,
coherence fast paths, CE/DE concurrency, stage merging, DCC/HTILE decode) is genuinely
deferred and must **not** be built ahead of the increment that needs it.

---

## Increment → architecture-seam mapping

| Increment | New/changed code | Seam(s) it touches |
|---|---|---|
| Present-only (today) | display loop calls `backend.present()` | **`GpuBackend`** (present+import only) |
| Gnm boot — task-20 | `libsceGnmDriver` HLE (libs) → `GnmDriver` (gnm) | HLE glue; `GnmDriver` records submit ranges |
| PM4 decode+trace — task-21 | `pm4/decode.rs`, `trace.rs`; `ExecMode::TraceOnly` | **PM4 decoder** + executor loop (no backend) |
| Trace corpus — task-22 | example ELFs (corpus) | exercises decoder/trace, embedded shaders |
| Present/sync | executor present/sync arms; `RunCommandList` channel msg; EOP→equeue | executor; `GpuBackend::{present,signal_eop}`; **`GpuState` regs** |
| Embedded-shader draw — task-24 | executor draw arms; `EmbeddedShaderProvider`; minimal `ResourceCache` | **`ShaderProvider`**, **`ResourceCache`**, `GpuBackend::{draw,bind_*,get_or_create_pipeline}`, `GpuState` views |
| GCN shader path | `ps4-gcn` (interpreter→recompiler); `GcnShaderProvider`; full cache | **`ShaderProvider`** (2nd impl), **`ResourceCache`** (textures/RT/readback), **`DirtySource`** |

## Introduce-now vs defer (prioritized)

**Introduce NOW (before task-20 code):**
1. **`GpuBackend` trait in `ps4-core`, present+import surface only, ash the sole
   impl.** Rationale: stops raw ash from spreading into the PM4 executor — the
   actual calcification risk. Cheap (one impl), high leverage.
2. **`ps4-gnm` crate skeleton (Vulkan-free) with the decode/state/exec module
   split.** Rationale: fixes the dependency direction and headless-testability from
   line one; retrofitting a crate split after PM4 code lands is expensive.
3. **`ShaderProvider` and `ResourceCache`/`DirtySource` *trait* stubs** (not impls).
   Rationale: they shape `PipelineKey`/`ResourceKey` and the backend's
   upload/import surface; defining the seams early keeps the embedded-shader draw from
   hardcoding "two shaders" / "no cache".

**Defer until its increment actually arrives:**
- Any second `GpuBackend` impl (Metal) or `#[cfg]` backend switch — until MoltenVK
  runs and Metal is scheduled.
- Command-encoder / render-graph / allocator abstractions — never, unless a real
  backend forces them.
- `ps4-gcn` contents — until the GCN shader path.
- `mprotect` dirty tracking / x86jit watched-range consumption — until the
  embedded-shader draw's conservative invalidation becomes a bottleneck.
- RT readback / RT-as-texture resolve — until a guest exercises them (the GCN shader path).

---

## (a) Recommendation: a pre-task-20 "backend abstraction" prerequisite task — YES

**Do it.** A single, low-risk refactor task should land **before** any task-20 code.
Without it, task-20→24 will be written directly against `VulkanContext`'s inlined
present path and the one-big-struct state, and the backend trait will have to be
retrofitted through PM4 code that already assumes raw ash — the exact rewrite this
design exists to prevent.

*Task sketch (do NOT create — for the maintainer):* **"gpu: extract GpuBackend trait
(present-only) + carve ps4-gnm crate skeleton."** Introduce a narrow `GpuBackend`
trait in `ps4-core` covering only what the present path needs today
(`present(target)`, `try_import_host_range`, target/resource creation stubs); move
the current softgpu blit + task-18 zero-copy import behind an `AshBackend` impl in
`ps4-gpu`; make `run_display_loop` drive the trait instead of open-coding
fence→acquire→copy→submit→present. Simultaneously create the empty Vulkan-free
`ps4-gnm` crate (`pm4/`, `state.rs`, `exec.rs`, `shader/`, `cache/` skeletons) with
the `ShaderProvider`/`ResourceCache`/`DirtySource` trait stubs. **Zero behavior
change** — same present, same 60fps, same zero-copy/staging fallback, verified by
the existing softgpu example. This establishes every seam the incremental GPU work plugs
into, before the first PM4 line is written.

## (b) Non-goals / over-engineering traps to avoid

1. **Fantasy GPU-HAL.** Do NOT build a portable command-encoder / barrier / render-
   graph / descriptor abstraction modeled on no real second backend. Draw the
   `GpuBackend` trait at PS4/PM4 granularity (present/draw/bind/target), keep all
   Vulkan verbs inside the ash impl, and grow the trait one method per increment. One
   impl is the target until MoltenVK+Metal are real.
2. **Premature invalidation machinery.** Do NOT build `mprotect` signal handlers or
   a full page-dirty subsystem for the embedded-shader draw. Use conservative
   "re-upload referenced ranges each submit" (the corpus is one draw). Only build the
   real `DirtySource` (ideally an x86jit watched-range API) when the GCN path's texture
   cache needs it.
3. **Monolithic state struct / big-bang crate.** Do NOT grow one `VulkanContext`-
   style struct to hold PM4 state, and do NOT try to land PM4+state+shaders+cache in
   one go. State is a sparse register file plus additive derived views; the work is
   a strict sequence of small, each-shippable steps that keep softgpu present alive.

## (c) Open questions for the maintainer

1. **x86jit watched-data-range dirty API (blocking for the GCN cache).** Confirmed:
   x86jit today only dirty-tracks pages tagged as *code* (`mark_code` →
   `take_dirty_code`); general data-page writes are not recorded, and
   `MemConsistency::Fast` is in use. The resource cache's precise invalidation needs
   a *new* x86jit facility: register watched data ranges + drain-dirty. **Should we
   file an x86jit backlog task for a `watch_range`/`take_dirty_ranges` API** (per the
   "x86jit changes go through its backlog, then bump the rev pin" workflow), or
   accept conservative per-submit re-upload indefinitely for the near term?
2. **GPU thread model.** OK to keep the display thread owning the Vulkan device and
   ship backend commands to it over the crossbeam channel (recommended, preserves
   today's present path), rather than a dedicated async GPU thread?
   The latter is deferred; confirm that's acceptable.
3. **Crate granularity.** Three new crates (`ps4-gnm`, `ps4-gcn`, and the reshaped
   `ps4-gpu`) vs. keeping `ps4-gcn` as a module inside `ps4-gnm` until the GCN path
   lands. This doc recommends a separate `ps4-gcn` crate — acceptable,
   or prefer to defer even the crate until then?
4. **RT readback scope + per-title config.** Do any near-term corpus/target guests
   read a render target back on the CPU, or is readback safely a GCN-path-only concern
   (affects when the `backend.readback` method must exist)? Readback ships **gated OFF**
   (env lever now, `ReadbackPolicy`); **per-title readback config is a future
   workstream** (a per-game "write color buffers" toggle) — confirm the
   env-now / per-title-later split.
5. **onion/garlic memory-type flag threading (memory-manager ↔ GPU-cache dep, C5).**
   The resource cache wants a per-range coherence flag (onion vs garlic) to pick
   zero-copy vs copy+track policy. That flag originates in `sceKernelAllocateDirectMemory`
   / map-with-cache-attributes in `ps4-memory`/kernel. Is threading a memory-type flag
   from the memory manager into the GPU cache acceptable now (as a hint field), or
   deferred until the GCN path makes the distinction matter?
6. **MoltenVK sync + queue support (C1, C2).** MoltenVK exposes few Vulkan queues and
   variable timeline-semaphore support. Confirm the policy: map multiple PS4 rings onto
   available queues with graceful serialization, and gate timeline semaphores behind a
   capability check with a fence/label-polling fallback — both per `decision-3`'s
   gate-and-fallback rule.

---

*Companion decision: `decision-4` (all GPU work flows through the `GpuBackend`
trait + the incremental seam split) records the cross-cutting commitment this doc
implies.*
