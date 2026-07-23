---
id: doc-6
title: Retail GNM bring-up — discovery log (how the GPU path was reverse-engineered)
type: other
created_date: '2026-07-15 08:35'
---

# Retail GNM bring-up — discovery log

**What this is.** A running *discovery journal* for connecting a real retail title's
graphics (PS4 **GNM** — the low-level Liverpool/GCN graphics API) to unemups4's existing GPU
pipeline. Unlike doc-4 (the *method*) and doc-5 (the *debugging casebook*), this doc records
**how we came to understand the GPU path**: what we already had, what the title actually asked
for, how we found the seam between them, and — for each new mechanism — *what led us to look
where we looked, and what the mechanism actually does*.

**Audience.** Someone who wants to bring a retail title's rendering up on an HLE emulator and
does not yet know how the console's graphics submission model works, nor how it maps onto a
software GCN→SPIR-V→Vulkan pipeline. Read doc-4 first for the smoke-loop method; this is the
GPU-specific companion.

> **Keep this current.** Append a dated entry every time you uncover a *new mechanism* (a
> submission path, a PM4 opcode class, a shader stage, a sync primitive) or a *surprise* (the
> hardware/SDK doesn't behave the way the obvious mental model said). Write it as: **the trigger**
> (what symptom/question sent you looking), **the hunt** (how you found the answer — which file,
> log, disasm, or list), **the mechanism** (what it actually does), and **the consequence** (what
> you now build differently). That narrative is the value; a bare "added X" line is not.

---

## Entry 0 — Framing: two ways graphics can be "missing"

**Trigger.** The Celeste boot smoke loop (see the epic + doc-5) marched through audio, input,
and save-data, then — right after `Graphics::GraphicsSystem::Initialize` ran and
`sceVideoOutOpen` succeeded — stopped at `FATAL: missing symbol sceGnmAddEqEvent`. So graphics
*init* worked but the *first GNM driver call* was unimplemented.

**The question that framed everything.** Before writing a single stub, one question decides the
whole approach: **do we already have a GPU pipeline, or are we building one?** An HLE emulator
can be missing graphics in two very different ways:

1. *No pipeline* — nothing turns guest GPU commands into pixels. Then the retail wall is the
   least of your problems; you build a renderer first.
2. *Pipeline exists, wrong front door* — there is a working path from GPU command buffers to
   the screen, but it is reached through a different entry point than the one this title uses.

These demand opposite moves. (1) is months of GPU work. (2) is *plumbing*: find the existing
entry function and route the title's calls into it. Getting this wrong wastes enormous effort
— you would reimplement a renderer that already exists, or stub a front door that leads
nowhere. So the first job was **not** to implement `sceGnmAddEqEvent`; it was to find out which
world we were in.

**How we answered it.** Two cheap probes, before any code:

- *What does the title actually import?* The loader logs every unresolved GNM symbol it
  link-stubs. One run, grep `Stubbed missing: sceGnm`, gave the **complete GNM surface Celeste
  needs** — ~70 functions. Reading the names is already a design document: `CreateWorkloadStream`,
  `BeginWorkload`, `SubmitCommandBuffersForWorkload`, `DingDongForWorkload` — a *workload*
  vocabulary — plus `DrawIndexOffset`, `SetPsShader350`, `AddEqEvent`. The shape of the import
  list tells you the submission model before you read any of our code.
- *What do we already have?* A recon pass over `crates/gnm` and `crates/gpu` (and crucially the
  shipped `examples/ps4-gcn-triangle`) answered world (2): a **complete register-route pipeline
  already renders real GCN end-to-end** — for the examples. That reframed the entire task from
  "build GNM" to "route Celeste's workload submits into the executor we already have."

**Consequence.** The plan (task-113.4.1) is *pull-driven plumbing*, not a GPU rewrite: build the
missing front door (the workload API + event/completion glue), let the existing executor run on
the title's real command buffer, and then — only then — implement the specific PM4 decode arms
and shader emitters the title actually emits. Everything below is the detail behind that pivot.

---

## Entry 1 — How PS4 graphics submission actually works (the GNM model)

**Trigger.** The import list was full of a vocabulary the examples never used — *workload*
streams, *DingDong*, `Submit...ForWorkload`. To route these into our pipeline we first had to
understand what they *are*, and how they relate to the plain `sceGnmSubmitCommandBuffers` the
examples do use.

**The hunt.** Three sources, cross-checked so we never rely on any one (and never copy another
emulator's source): (a) the SDK header names + our own `data/oo_sdk/include/orbis/GnmDriver.h`
for the *shapes* of the calls; (b) our examples' `main.c`, which hand-build the PM4 stream and
so *document the console's command format* in code we own; (c) our existing executor
(`crates/gnm/src/exec.rs`) and PM4 decoder (`crates/gnm/src/pm4/`), which show exactly which
part of the model we already interpret. The model below is reconstructed from those, not lifted
from a reference implementation.

### The layers, top to bottom

**1. The command buffer is the real interface.** The GPU does not have a "draw a triangle"
function. It consumes a linear byte stream of **PM4 packets** — the AMD command-processor
format. A PM4 packet is a header dword (a type, an opcode like `IT_DRAW_INDEX_AUTO` or
`IT_SET_CONTEXT_REG`, and a length) followed by payload dwords. A frame is: write a big PM4
stream into a **DCB** (draw command buffer, graphics) and optionally a **CCB** (constant
command buffer, compute/constant updates), then hand the GPU those buffers' addresses and
sizes. Everything else in GNM is a helper that *produces* PM4 or *submits* it.

**2. Who writes the PM4 — a split that matters enormously for HLE.** This was the key
realization, and it is *not* obvious from the outside:

- Most PM4 is written by **guest code**. On a retail title, Sony's `libGnm`/`libGnmx` are
  **statically linked into the title's own binary**. So `sceGnmDrawIndex`, the `SET_*_REG`
  state packets, `CLEAR_STATE`, `CONTEXT_CONTROL` — the guest builds all of these *itself*,
  writing dwords into its own DCB. We never see those as imports, and we do not implement them
  as functions — we only have to **decode** the resulting PM4 in the executor. (Our examples
  prove this: `examples/ps4-gcn-triangle/main.c` literally does
  `*cmd++ = pm4_type3(IT_DRAW_INDEX_AUTO, 2)` by hand — that is exactly what a retail title's
  statically-linked Gnm does internally.)
- A *few* things are written by the **driver** (`libSceGnmDriver.so`), which is a real system
  module the title imports and which we must HLE. These are the calls that either (a) need
  privilege/kernel cooperation (the actual *submit* — handing buffers to the GPU ring), or (b)
  translate a rich C struct into PM4 that the guest can't trivially inline. The prime example
  of (b) is **shader binding**: `sceGnmSetVsShader(cmd, &VsStageRegisters)` takes a *struct* of
  shader register values and emits ~29 dwords of `SET_SH_REG`/`SET_CONTEXT_REG` into the guest
  cmdbuf. So `SetVsShader`/`SetPsShader` (and Celeste's `SetPsShader350`/`UpdateVsShader`
  variants) are **HLE PM4 emitters** — we implement them to write the right packets. We already
  do this for the base VS/PS in `crates/gnm/src/pm4/emit.rs`; the retail variants extend the
  same pattern.

  > The one-line test for "emitter vs decode-only": *does the call take a struct the guest
  > couldn't cheaply turn into PM4 itself?* Shader-register setup → yes, HLE emitter. A plain
  > `DrawIndex(count)` → no, the guest inlines the DRAW packet; we only decode it.

**3. Submission — plain vs workload.** Once the DCB/CCB are built, they must reach the GPU.
There are two front doors, and Celeste uses the newer one:

- **Plain** (`sceGnmSubmitCommandBuffers(count, dcbAddrs[], dcbSizes[], ccbAddrs[],
  ccbSizes[])`): hand N command buffers straight to the graphics ring. This is what our
  examples call, and what our HLE already implements (`libscegnmdriver/submit.rs`): it reads
  the addr/size arrays out of guest memory into a `SubmitRange` and runs the executor on it.
- **Workload** (`CreateWorkloadStream` → `BeginWorkload`/`EndWorkload` bracketing →
  `SubmitCommandBuffersForWorkload` / `SubmitAndFlipCommandBuffersForWorkload` →
  `DingDongForWorkload`): a thin bookkeeping layer *around the same submit*. A **workload
  stream** is an opaque id the driver uses to track a group of submits for scheduling/profiling
  and for the compute "ding-dong" doorbell (the mechanism that rings the GPU that new work is
  queued on a ring). For our purposes — a software GPU with no real rings or scheduler — the
  workload wrapper carries **no semantics we must honor**: `Begin`/`End` are bookkeeping,
  `Create/DestroyWorkloadStream` hand back/release an opaque id, `DingDong` is a doorbell we
  can treat as a no-op, and **`SubmitCommandBuffersForWorkload` is `sceGnmSubmitCommandBuffers`
  with an extra leading workload-id argument.** That is the whole insight that makes the front
  door cheap: *the workload family funnels into the exact same `SubmitRange` → `Executor::run`
  path the plain submit already uses.* The `...AndFlip...` variant additionally requests a
  present (a flip) after the submit, same as the plain `SubmitAndFlip`.

**4. Completion + flip — how a frame ends.** The GPU runs asynchronously on real hardware, so
the title needs to know *when a submit is done* (to reuse buffers, advance a frame) and *how to
show the result*:

- **Completion signalling.** Two mechanisms coexist. (a) PM4-embedded: the command stream ends
  with an `EVENT_WRITE_EOP`/`EOS` packet that writes a label value to a guest address when the
  GPU reaches it — our executor already honors these (`write_eop_label`/`write_eos_label`),
  which is why the examples work *synchronously* (we finish `Executor::run`, then the label is
  already written). (b) **Event queues** (`sceKernelCreateEqueue` + `sceGnmAddEqEvent`): the
  title registers a GPU-completion event on an equeue and *blocks on the equeue* until the GPU
  signals. Retail engines gate their frame loop on this. Today our equeue is a stub and
  completion is effectively synchronous — so the open question for Phase A is whether Celeste
  *waits* on an equeue event that must actually fire. `AddEqEvent` being the very first GNM wall
  is the hint that it does.
- **Flip (present).** Rendering draws into an off-screen color buffer; **flip** makes it the
  scanned-out front buffer. The title registers its buffers with `sceVideoOutRegisterBuffers`,
  then `sceVideoOutSubmitFlip(index)` (or the `SubmitAndFlip` submit variant) asks to present
  buffer `index`. Our pipeline already wires this: the flip crosses a channel to the Vulkan
  display thread and blits/presents. `sceVideoOutGetFlipStatus` lets the guest's present-poll
  loop advance.

### The seam we're building onto

Putting it together, the existing path (proven by the examples) is:

```
guest builds PM4 (draws + state)  ─┐
HLE emits shader-setup PM4        ─┴─►  DCB/CCB bytes in guest memory
        │  sceGnmSubmit[AndFlip]CommandBuffers(...)
        ▼
  record_submit()  →  SubmitRange{dcb,ccb,sizes,flip}   (libscegnmdriver/submit.rs)
        ▼
  ps4_gnm::exec::Executor::run(&SubmitRange)             (crates/gnm/src/exec.rs:139)   ◄── THE ENTRY
        │   decode PM4 packet stream
        ├─ SET_*_REG        → GpuState shadow register file
        ├─ DRAW_INDEX_AUTO/2 → resolve bound VS/PS  →  GCN .sb  →  ps4_gcn::recompile → SPIR-V
        └─ EVENT_WRITE_EOP  → write completion label
        ▼
  PresentSink (crates/gpu) → Vulkan backend → flip → window
```

**Consequence for the build.** The retail front door is: HLE the **workload** family so
`Submit*ForWorkload` constructs the same `SubmitRange` and calls the same `Executor::run`; make
`AddEqEvent`/equeue actually signal completion if Celeste blocks on it; stub the caps/validate/
state-init getters; then let the executor run on Celeste's *real* command buffer and **log
which PM4 opcodes and shader-set calls it actually emits**. Only those get new decode arms /
emitters (Celeste is 2D MonoGame — expect textured-quad `DrawIndex` + VS/PS, and *probably not*
compute/geometry/tessellation, so we don't build those until pulled). The executor and the
GCN→SPIR-V recompiler are already there; we are wiring a title into them, not writing a GPU.

---

## Entry 2 — Does Celeste block on an equeue GPU-completion event? (Phase A)

**Trigger.** Entry 1 §4 left one thing genuinely open: retail engines *can* gate their
frame loop on a `sceKernelWaitEqueue` that only returns when the GPU signals a completion
event registered via `sceGnmAddEqEvent`. Our executor is synchronous, our equeue was a
pure stub (`sceKernelCreateEqueue` returned 0 and wrote nothing; `sceKernelWaitEqueue`
slept 16 ms and returned 0). If Celeste truly *blocked* on a real completion, that stub
would either deadlock it or let it race ahead of "GPU" work. `sceGnmAddEqEvent` being the
very first GNM wall (Entry 0) was the hint it mattered. So before wiring anything, the
question was: **build the completion signal, then instrument to see whether the guest
actually waits and whether the signal is what unblocks it.**

**The hunt.** Two cheap probes rather than guessing. (a) Grep the tree for any equeue
event-data getter (`GetEventData`/`GetEventId`/`SceKernelEvent`) — **none exist**, so the
guest can't be reading a structured event payload we'd have to synthesize. (b) Add the
three GNM eq handlers + a minimal completion registry (`libkernel/equeue.rs`), wire the
submit-done path to `signal_gpu_completion`, and make `sceKernelWaitEqueue` log whether any
GNM event is registered and whether a completion was pending when it waited.

**The mechanism (what the run showed).** Celeste calls, in order right after
`sceVideoOutOpen`: `sceGnmAddEqEvent eq=1 type=64 id=0x0` — and then **does not reach a
single `sceKernelWaitEqueue` before it faults elsewhere** (see Entry 3). So for the boot
path we've observed, the equeue-completion machinery is *registered but never waited on
yet*: the frame loop that would block on it lives past a wall the guest hits first. The
`type=64` is the GNM end-of-pipe event class; `id=0x0` is the guest's cookie. Crucially,
because our executor is synchronous, the shape we built (register → mark-triggered on
submit-done → report from wait) is *inherently correct* even once the guest does wait: the
completion is always already pending by the time a wait could observe it, so a synchronous
"already done" answer can never be wrong here — there is no window where the guest could
see an in-flight submit.

**Consequence.** The equeue glue is deliberately minimal and *shaped to degrade safely*:
`sceKernelCreateEqueue` now hands back a stable non-zero handle (a guest that keys events on
the returned handle can match them); `AddEqEvent`/`DeleteEqEvent`/`GetEqEventType` track the
event in a process-global registry; `sceGnmSubmitDone` / `RequestFlipAndSubmitDone` signal a
completion; `sceKernelWaitEqueue` drains one pending completion and reports a single
triggered event (count written to the out-param), else falls back to the pre-existing 16 ms
VSync sleep so a spin-wait doesn't burn CPU. We did **not** synthesize a full
`SceKernelEvent` payload — nothing reads one yet, and inventing a wrong struct layout would
be worse than writing none. The instrumentation stays in so the *next* session sees the
moment Celeste first actually waits, and whether reporting a bare count is enough.

---

## Entry 3 — The workload front door works; the wall is now a CPU-lift gap, not GNM (Phase A)

**Trigger.** With the workload submit family, equeue glue, and state-init/caps stubs in,
the question was simply: does the guest now march through GNM init and reach its first
`Submit*ForWorkload` + flip, so the executor runs on Celeste's *real* command buffer?

**The hunt.** One smoke-loop run (`UNEMUPS4_BACKEND=interp`, the clean-guest-fault backend)
with `RUST_LOG=info`, grepping for the GNM/EQ/submit/flip/fault lines.

**The mechanism (the wall progression).** The front door did its job: the boot marched
**past** every GNM wall that used to stop it. `sceGnmAddEqEvent` — the original Entry-0
FATAL — is now a clean handled call. One stub outside GNM surfaced immediately after it and
had to be added to keep advancing: `sceKernelIsNeoMode` (the guest branches its video-out
setup on PS4-Pro-vs-base; we return 0 = base PS4). Then the guest faulted — but **not on
anything GNM**. It hit a guest **CPU-lift gap**: `vextractps $0x2,%xmm0,0x2c(%rsp)` — a
VEX-encoded AVX instruction (bytes `c4 e3 79 17 44 24 2c 02`) the x86jit interp backend
does not yet lift — inside the `scePlayStation4` interop module (VMA `[0x1584000,0x160b000)`
+0x57ab) during managed-runtime setup. **No GNM submit ever reached the executor**: the
fault is upstream of the first `Submit*ForWorkload`, in runtime plumbing, not graphics.

**Consequence.** Phase A's front door is *complete and correct for what the guest exercised*
— but the next wall is out of GNM's hands entirely. Per the x86jit-changes-via-backlog rule,
the AVX `vextractps` lift is a task for the **x86jit backlog**, not an edit here. Two more
findings worth carrying forward:

- **The unhandled-opcode instrumentation is live and safe.** The regression run of
  `examples/ps4-gcn-triangle` logged `unhandled PM4 opcode 0x28 (IT_CONTEXT_CONTROL)` and
  `0x10 (IT_NOP)` — both correctly *skipped, non-fatal*, and the triangle still rendered and
  flipped. So when Celeste's command buffer does reach the executor, its real draw/shader/PM4
  mix will surface as `[GNM] unhandled PM4 opcode 0xNN (NAME)` lines, one per distinct opcode
  per submit — the pull-driven seam is ready and proven not to disturb the working path.
- **The workload wrapper carried no surprises.** Everything Entry 1 §3 predicted held: the
  `Submit*ForWorkload` calls are the plain submit plus a leading id, and the lifecycle calls
  (`Create/Begin/End/DingDong/RequestFlipAndSubmitDone`) are bookkeeping. Nothing in the
  workload path needed semantics a software GPU must honor. The one ABI note: the AndFlip
  workload variant's trailing videoout/flip args live on the stack (past the 6 register
  args), so — exactly like the plain AndFlip handler — we read only the 6 register args and
  set `flip=true`; the present crosses through the sink, not those args.

*(Next entries: the first PM4 opcodes Celeste's command buffer actually emits — once the
CPU-lift wall past scePlayStation4 is cleared and a submit reaches the executor.)*

---

## Entry 4 — SetPsShader350 is the base emitter with a version tag; the GNM path is fully unblocked, the wall is now Mono (Phase B)

**Trigger.** The x86jit AVX/div CPU-lift gaps (Entry 3's `vextractps`, plus a `div r/m8`)
were landed upstream, so the boot marched past the scePlayStation4 runtime plumbing and hit
the *first real graphics wall*: `FATAL: missing symbol sceGnmSetPsShader350 [NID 5uFKckiJYRM]`,
the SDK-3.50 variant of pixel-shader binding, right after `sceGnmSetVsShader` succeeded. The
question: is "350" a different struct / an extra modifier arg, or just an ABI tag on the same
`PsStageRegisters` emitter we already have?

**The hunt.** One header lookup settled it before any code. `data/oo_sdk/include/orbis/
GnmDriver.h` declares them with **identical signatures**:
`sceGnmSetPsShader(uint32_t* cmd, uint32_t numdwords, const void* psregs)` and
`sceGnmSetPsShader350(uint32_t* cmd, uint32_t numdwords, const void* psregs)` — both with the
"numdwords must be 40" constraint. So "350" is purely an ABI-version tag; the struct, the
dword count, and the emitted PM4 are the same. (This mirrors the workload-vs-plain-submit
insight from Entry 1: a scary-looking retail variant that is the familiar call plus a label.)

**The mechanism.** `sceGnmSetPsShader350` reuses `emit::set_ps_shader` verbatim — same 40-dword
`SET_SH_REG`/`SET_CONTEXT_REG` stream, same `PGM_HI`-forced-to-0 retail invariant. A regression
test emits both the base and the 350 variant for the same regs block and asserts the PM4 is
**byte-identical**, so a future divergence in either emitter is caught. The likely siblings
`sceGnmUpdatePsShader350` / `sceGnmUpdateVsShader` were added the same way: on our register-route
model an "update" and a "set" both just re-write the `SPI_SHADER_PGM_*` / pipeline-state PM4
(registers are the truth — the shadow bank is overwritten either way), so Update shares the Set
emitter path. Celeste calls the 350 PS variant, not the base one.

**What the run then showed — and the surprise.** With SetPsShader350 in, the GNM path unblocked
*completely* for what Celeste's boot exercises: it marched
`sceGnmSetVsShader` → `sceGnmSetPsShader350` → `sceGnmDrawIndexAuto count=3` with **zero further
GNM walls**. Two things worth carrying forward:

- **Celeste's early draw is HLE'd, not a submit.** That `DrawIndexAuto count=3` (a 3-vertex
  warmup/clear draw during graphics init) came in as the *HLE function* `sceGnmDrawIndexAuto`,
  which records into the driver — **not** as PM4 in a submitted command buffer. And crucially,
  **no `Submit*` / `Executor::run` fired before the boot faulted.** So the executor has still not
  run on a Celeste-built command buffer: the shader-set + first draw all happen *before* the
  first frame submit, which lives past the current wall. AC#4/#5 remain unexercised — not because
  the GNM front door is missing anything, but because the boot doesn't reach the submit.

- **The wall moved out of GNM entirely — twice.** Immediately past the first draw, the next
  FATAL was `sceNpTrophyCreateContext` — the achievement API, not graphics. Stubbing the 7
  trophy calls Celeste imports (`Create/Destroy{Context,Handle}`, `RegisterContext`,
  `GetTrophyUnlockState`, `UnlockTrophy`) as boot-unblock success-stubs (the `Create*` hand back
  a non-zero opaque id via out-ptr; same shape as the Phase-A `sceKernelIsNeoMode` unblock)
  advanced the boot ~150 ms further — into a **guest-side fault inside the Mono managed runtime**:
  `UnmappedMemory (read) of 0x1d`, `cmpb $0x0,(%rdi,%rdx)` at `eboot.bin+0x104620`, a null-base +
  0x1d struct-field read whose *entire* rbp backtrace is inside `eboot.bin`, accompanied by
  `mono_os_mutex_lock: "Resource deadlock avoided"`. This is Mono runtime/thread setup, not a
  GNM, recompiler, or Vulkan issue.

**Consequence.** The pixel-shader emitter (the GNM deliverable of this session) is done: SetPsShader350
+ the Update siblings are real emitters sharing the proven `emit::set_ps_shader`/`set_vs_shader`
path, and the triangle regression still renders+flips on both backends. But the frame that would
drive the executor lives past a **Mono-runtime null deref** in the guest's own statically-linked
code — a CPU/runtime wall, not graphics. That is the STOP point for the GNM track: reaching AC#4/#5
now depends on the Mono bring-up, not on any further GNM emitter or decode arm. (When the boot does
reach a submit, the `[GNM] unhandled PM4 opcode 0xNN` instrumentation from Entry 3 is still armed to
reveal Celeste's real command-buffer opcode mix.)

## Entry 5 — Celeste's draws live in the HLE draw builders, NOT in the submitted DCB — and the wall is now the GCN recompiler

**Trigger.** The Mono/memory walls of Entry 4 were cleared upstream (mutex-type + 1 MB-align fixes,
plus `sceKernelGetdents`), so the boot finally reached a **full render loop**: ~973 frames, each
`SetVsShader`/`SetPsShader350` + `DrawIndexAuto`/`DrawIndexOffset` + one
`sceGnmSubmitAndFlipCommandBuffers` (a 4 MB DCB) + `sceGnmSubmitDone`. `Executor::run` fires per
submit — but **no frame rendered** (a `UNEMUPS4_DUMP_PNG` dump of a mid-run frame was pure white).
The question: why does a running executor over a 4 MB retail DCB produce nothing?

**The hunt.** `UNEMUPS4_PM4_TRACE=1` on the real submits gave the packet histogram across the run:
**7612 `SET_CONTEXT_REG`, 930 `SET_SH_REG`, NOP/DMA_DATA/`SET_UCONFIG_REG` — and *zero* draw
packets** (no `0x2D`/`0x27`/`0x35`/dispatch). Two DCB shapes alternate: a **4 MB** buffer submitted
at full capacity but **zero-filled** (decodes as ~524 288 garbage `T0 base=0x0` packets — a
pre-allocated arena the guest never trimmed), and small **3–5 KB** DCBs that carry real state + the
shader-set PM4 but, again, **no draw**. Meanwhile `RUST_LOG=ps4_libs=info` showed the guest calling
`sceGnmDrawIndexOffset` **3839×** and `sceGnmDrawIndexAuto` **974×** — through the *HLE entrypoints*,
not as PM4.

**The mechanism (the split).** The `sceGnmDraw*` functions are, on real hardware, gnmx *builders*
that write a draw PM4 packet into the caller's command buffer; the guest submits that same buffer.
Our HLE `sceGnmSetVsShader`/`SetPsShader` builders already do this (that's why the DCB has 930
`SET_SH_REG` — the shader-set PM4 is emitted into the guest cmdbuf, Entry 4). But the **draw**
builders were record-only no-ops (`drv.draw_index_auto()` → `let _ = …`). So Celeste's DCB got the
state + shader PM4 but **never got the draw packets** — the guest doesn't hand-write them (it's
HLE-linked and relies on the builder), so the executor decoded a buffer with binds but nothing to
draw. Per-frame the calls interleave `[VS bind][PS bind][draw][VS bind][PS bind][draw×N]…` then one
submit — a MonoGame `SpriteBatch`: multiple shader-bind→draw groups accumulated into one cmdbuf,
one flip per frame. Because the binds are *interleaved with* the draws in the stream, a side-channel
"replay the shadow draws at submit time" bridge would collapse every draw onto the *last* bound
shader — wrong. **The draw must land in the cmdbuf in stream order.**

**The fix.** Make the draw builders emit their PM4 into the caller's `cmdbuf`, exactly like the
shader-set builders (`emit::draw_index_auto` → `IT_DRAW_INDEX_AUTO`, `emit::draw_index_offset` →
`IT_DRAW_INDEX_OFFSET_2` 0x35), plus the missing `0x35` executor decode arm
(`dispatch_draw_index_offset`, which reads the index base/type from the bound `IT_INDEX_BASE`/
`IT_INDEX_TYPE` state and pulls the offset sub-range). The draw now decodes in order after the
binds that precede it, and `setup_draw` resolves it against exactly those binds. **No corpus/triangle
regression:** those examples call `sceGnmDrawIndexAuto` and *also* hand-emit the identical packet at
the same cursor — the builder's write is overwritten byte-for-byte (triangle passes `cmd`) or lands
harmlessly at the buffer head with no shader bound → clean `Unbound` defer (pm4-test passes `dcb`).
The pink triangle still renders.

**What the run then showed — the new wall.** With the draws emitted, the executor's draw arms now
fire on Celeste's real geometry (`DrawIndexAuto count=3`, `DrawIndexOffset count=6/300/…`) — AC#4 is
exercised: the executor runs on a Celeste-built command buffer at last. Every draw defers at **one**
point: `ShaderPairResolution::NeedsGcn` — "bound to a non-recompilable (.sb GCN) shader — deferring
draw". Celeste's VS/PS resolve to real `.sb` GCN program addresses (via the SH-register route), but
the GCN→SPIR-V recompiler can't yet compile *these* shaders. That's a clean, recognized defer, not a
crash, and the frame stays blank (white).

**Consequence.** The submit/decode/draw front door is now complete for a retail managed-runtime
title: state, shaders, and both draw variants all reach the executor in stream order and resolve up
to shader recompile. The wall is no longer "the draw is missing" — it's **the GCN recompiler on
Celeste's real shader bytecode** (GPU-track work: tasks 55/58/47/72). This is the first time a
retail title's own command buffer has driven the executor's draw path. The `NeedsGcn` defer log is
the pull-driven seam for the recompiler work: it names, per draw, exactly which shader class must
compile next for the first pixel to land.

## Entry 6 — the `NeedsGcn` wall was the `.sb` **parser**, not the recompiler; the OrbShdr footer has an input-usage gap (2026-07-15)

**Trigger.** Entry 5 concluded the wall was "the GCN recompiler on Celeste's real shader bytecode".
Before writing a single recompiler instruction, dump the bytecode and confirm which layer actually
fails — `ShaderPairResolution::NeedsGcn` is returned whenever the provider chain returns `Err`, and
the recompiler is only *one* `Err` source; `parse_sb` is another, upstream of it.

**The hunt.** New env-gated diagnostic `UNEMUPS4_DUMP_GCN=<dir>` dumps the register-derived shader
window (`SPI_SHADER_PGM_LO/HI` → address, [`pgm_addr`]) to disk on each resolve, **before** parse —
the point is to capture shaders that fail to parse. One `RUST_LOG=warn` run made the cause uniform
and unambiguous: `GCN shader parse rejected … m_length does not match header offset`. The recompiler
had **never run**. The `NeedsGcn` category had named the last observer (the chain), not the failing
layer (the parser).

**The mechanism (RE'd from 22 dumped shaders).** Our validator required the `OrbShdr` footer to sit
tight against the code: `code_start + m_length == header_addr`. Real Orbis shaders lay out as
`[ GCN code (m_length bytes, ends in s_endpgm) ][ input-usage/hash table: 8..64 B gap ][ OrbShdr
footer ]`, each shader 256-byte-aligned. So `header_addr = code_start + m_length + gap` — the footer
is a *gap* past the code end, never tight against it. The `m_length` field is exact and describes
only the machine code (verified: `code[m_length-4..] == 0xBF810000 s_endpgm` on all 22). The
`.dis`/header fields (`m_numInputUsageSlots`, `m_chunkUsageBaseOffsetInDW`) describe the table that
lives in that gap.

**The fix.** Relax the validator to `code_end <= header_addr <= code_end + MAX_FOOTER_GAP` and set
the parsed code range to exactly the `m_length` bytes (excluding the gap, so the decoder never walks
the usage table). A relaxation that loose re-admits a false-positive `OrbShdr` sequence inside the
code, so guard it with the format's own discriminator: the last code dword must be `s_endpgm`
(a stray magic's coincidental `m_length` won't land on a terminator). Note a **fetch shader** is a
subroutine that returns via `s_setpc_b64`, *not* `s_endpgm` — it is referenced by the fetch layout,
never handed to `parse_sb` as a standalone `.sb`, so the terminator guard is correct only because
fetch shaders don't take this path.

**Consequence + the real next wall.** With parse fixed (and the universal `s_mov_b32 vcc_hi, imm`
prologue every retail `.sb` opens with modeled — validate+discard like `m0`), the recompiler finally
runs on real GCN code and hits its *actual* instruction gaps. An offline harness (dump → extract
per-shader code bins → `decode_all`+`recompile`, a sub-second loop vs a 2-minute guest run) mapped
the full first-wall histogram across all 22: **VOP2** `V_AND_B32`/`V_LSHLREV_B32`/`V_CVT_PKRTZ_F16_F32`
(int/bit + f16-pack for the `compr` MRT export), **SOP1** `s_mov_b64 sdst,exec` (exec-mask save, 9
shaders), **SMRD** `s_load_dwordx4/x8` (scalar constant-buffer loads, 4 shaders), and the deepest —
**`s_swappc_b64`** (the vertex fetch-shader call, 5 VS). That is the genuine GPU-track frontier for
the first pixel (task-113.4.2). Lesson for this log: a defer *category* can misname the failing
layer; dump the input bytes and read them before committing to the plan the category implies.

## Entry 7 — clearing the recompiler frontier: RE every opcode against llvm-mc *before* implementing (2026-07-15)

**Trigger.** Entry 6's offline histogram gives, per shader, the exact `Inst` that first fails —
e.g. `Vop3 { op: 287, … }`, `Vop3 { op: 323, … }`, `Vop1 { op: 53, … }`. The tempting shortcut is
to read the decoded *op number*, guess the mnemonic from memory ("287 is 0x100+0x1F, VOPC range, so
a compare"), and implement that. Three such guesses in the session-2 frontier notes were all
**wrong**: op 0x11F is `v_mac_f32` (multiply-accumulate, writes a VGPR), not a VOPC compare; op
0x143 is `v_mad_u32_u24` (24-bit integer MAD), not `v_add_f32`; op 0x1A0 is `v_fract_f32`, not a
"v_mul-ish" op. A decoded op *number* is not an op *identity*.

**The hunt.** `llvm-mc` is the ground truth, but *disassembly* is unsupported for the `amdgcn`/
`bonaire` subtarget (`LLVM ERROR: disassembly not yet supported for subtarget`). The workaround is
**forward-assembly**: write candidate mnemonics in `.s`, run
`llvm-mc -triple amdgcn -mcpu=bonaire -filetype=asm -show-encoding`, and read back the `; encoding:
[0x..,…]` bytes. Compute the op field from the encoding and match it against the decoder's reported
op number. When you don't know the mnemonic, sweep a family (all the transcendentals, all the VOP2
ALU ops, all the 3-src natives) and read off which one lands on the target number.

**The mechanism — the GFX7 op-decode key (verified, reusable).** For a VOP3 instruction the op
field is `bits[25:17]` of dword0, which factors cleanly by the encoding's byte-3 prefix:
`op = 0x100 | (byte2 >> 1)` when byte3 == `0xD2`, and `op = 0x180 | (byte2 >> 1)` when byte3 ==
`0xD3`. The 9-bit VOP3 op space is *ranged*: VOPC → `0x000`, VOP2-promoted → `0x100`, native-VOP3
(3-src) → `0x140`, VOP1-promoted → `0x180`. So a VOP1 op re-encoded as VOP3 to carry abs/omod lands
at `0x180 + vop1_op` (e.g. `v_fract` `0x20` → `0x1A0`), and a VOP2 op at `0x100 + vop2_op`
(`v_mul_f32` `0x08` → `0x108`, `v_mac_f32` `0x1F` → `0x11F`). Standalone VOP1 op = `byte1 >> 1`
(`v_sin` byte1 `0x6B` → `0x35`); standalone VOPC op = `byte2 >> 1` with byte3's low bit as the high
op bit (f32 cmps prefix `0x7C`, i32 cmps `0x7D`). This key turns "what is op N?" into arithmetic
plus one llvm-mc probe to confirm.

**Semantics that bite — model the GCN meaning, not the mnemonic.** Two ops carry non-obvious
contracts the oracle *and* the recompiler must both honour or the differential diverges: `v_sin_f32`
takes its argument in **revolutions** (`D = sin(2·π·S0)` — the `2π` is intrinsic; emit `Sin(TAU·x)`
with the *same* f32 `TAU` on both sides), and `v_mac_f32` uses the **destination as an implicit
accumulator** (`D = S0·S1 + D`, read-modify-write — the recompiler must load the old dst before
writing). GCN's `v_rcp`/`v_sqrt` are HW approximations; modelling the exact IEEE `1/x` / `sqrt`
keeps oracle==recompiler bit-for-bit (the sub-ULP HW deviation is invisible through the RT — a
documented, deliberate divergence).

**Consequence.** Cleared the entire *mechanical* remainder of the frontier this way (`v_sin`,
`v_mad_u32_u24`, `v_fract`/`v_mul`/`v_mac`/`v_cvt_pkrtz` in VOP3 form, `v_rcp`) — 7 → 10/22 shaders
recompile end-to-end. Each op followed the decision-3 discipline: interp oracle mirror + recompiler
emit + a differential golden whose expected exports are **hand-computed to be exact in f32** (pick
inputs where the math is representable — `sin(2π·0.25)=1`, `fract(2.25)·4=1`, `mad_u24(3,2,1)=7` —
so the analytic expectation is independent of any transcendental rounding) + a decode golden
cross-checked against llvm-mc bytes. What remains is *not* mechanical: the predication/VCC family
(compares → per-invocation bool, `v_cndmask`, `v_add_i32` carry) and the `s_swappc_b64` fetch-shader
call. Lesson for this log: for an ISA whose disassembler is unavailable, forward-assembly *is* the
disassembler — never ship an opcode identity you haven't round-tripped through `llvm-mc`.

## Entry 8 — predication is a *single bool* per invocation, not a 64-bit wave mask (2026-07-15)

**Trigger.** Entry 7 left the predication/VCC family as the non-mechanical remainder: the retail
pixel shaders emit `v_cmp_*` compares, `v_cndmask_b32` selects, and `v_add_i32` (which produces a
VCC carry-out). On real GCN, VCC and any SGPR-pair predicate is a **64-bit wave mask** — one bit per
lane of a 64-wide wavefront — and the naive fear was that reproducing it in SPIR-V meant modelling a
whole wavefront's worth of lanes and their masking. The recompiler had explicitly *rejected* every
VCC read (`special_bits` returned an error), so this looked like the hard structural wall.

**The hunt.** Two RE steps, no guessing. (a) Forward-assemble every candidate through `llvm-mc
-mcpu=bonaire -show-encoding` to pin the identities: standalone VOPC `v_cmp_lt_f32` = op1 /
`v_cmp_gt_f32` = op4 (f32 prefix 0x7C, op = byte2>>1); the **VOP3-form** VOPC (byte3 0xD0) writes an
*arbitrary SGPR pair* — its bits[7:0] field is the pair `sdst`, which our decoder had mislabeled as a
VGPR `vdst`; `v_cndmask_b32` = VOP2 op0 (implicit VCC) and VOP3 op0x100 (predicate = an arbitrary
sgpr pair or VCC in src2); `v_add_i32` = VOP2 op0x25=37. (b) Re-read *why* the recompiler is
one-invocation-per-thread: the wave64/EXEC masking the CPU oracle models is the GPU rasterizer's job,
not the shader body's — the recompiled SPIR-V runs **once per lane**.

**The mechanism (the insight that collapses the whole family).** Because recompiled SPIR-V is
*per-invocation = one lane*, a wave-level 64-bit VCC (or SGPR-pair) mask **degenerates to a single
`bool`** in the recompiler. There is no wavefront to mask — each invocation is exactly one lane, so
"the VCC bit for this lane" is just one boolean. That turns the scary predication family into three
trivial lowerings: a compare emits a normal SPIR-V `OpFOrdLessThan`-style comparison → `bool`; store
it against its destination (a `PredKey` = VCC or the SGPR-pair number); `v_cndmask` becomes
`OpSelect` on that bool; `v_add_i32`'s carry-out is `OpULessThan(a+b, a)` → bool. Straight-line code
(no CFG yet) means "the most-recently-stored bool for that key" is always the faithful value. The CPU
oracle, being wave-level and faithful, mirrors the *same* semantics into the real 64-bit `st.vcc`
(one bit per lane) and per-SGPR-pair state — so the differential still diffs a faithful wave against a
single-lane recompile, and they agree on the exports.

**Consequence.** The entire predication frontier (Frontier A: 6 shaders) fell to a ~1-bool-per-key
abstraction: `sh01` (v_add_i32), `sh15`/`sh16` (VOP3-VOPC / standalone-VOPC + cndmask) now recompile
end-to-end (10 → 13/22), and `sh02`/`sh11`/`sh12` marched *past* all their compares/cndmasks to new,
unrelated walls. Two carry-forwards. (1) The decoder subtlety is real and reusable: for **any**
VOPC-encoded VOP3 (op < 0x100), bits[7:0] are a scalar `sdst`, never a VGPR — decode it through the
general source-field decoder so `s[16:17]` / `vcc` land correctly. (2) The single-bool predicate model
is the **foundation for control flow**: `sh02`'s next wall is `s_or_b32 vcc, vcc, s16` (a scalar OR of
two predicate registers → `OpLogicalOr` of two `PredKey` bools) followed by `s_cbranch_execz` (branch
on the wave-empty predicate). Both are natural extensions of the bool-per-key map, not a new model.
Lesson for this log: before assuming a wave-level GPU primitive needs a wave-level implementation, ask
what it *degenerates to* under the recompiler's actual execution model — per-invocation single-lane
SPIR-V makes VCC a bool, not a mask.
## Entry 9 — the vertex fetch-shader call (`s_swappc_b64`) is a leaf-inline, not a new opcode (2026-07-15)

**Trigger.** Entry 6's offline histogram left 5 real Celeste VS (sh05/07/10/19/21) all walling on the
*same* first instruction — `Sop1 { op: 33, … }` = `s_swappc_b64` — the deepest, "structural" frontier
item (task-113.4.2 AC #7). Unlike every ALU op cleared in Entry 7, this one is not a value computation:
it is a **subroutine call**, and the callee (the *fetch shader*) is a *separate code blob* the main VS
stream doesn't even contain.

**The hunt — RE the call/return contract from the dumped VS.** llvm-mc forward-assembly first (Entry 7
discipline): `s_swappc_b64 s[0:1], s[0:1]` → `[0x00,0x21,0x80,0xbe]`, `s_setpc_b64 s[0:1]` →
`[0x00,0x20,0x80,0xbe]` — the op field is byte1 (`(w0>>8)&0xFF`): 0x21 = swappc, 0x20 = setpc; sdst =
`(w0>>16)&0x7F`, ssrc0 = `w0&0xFF`. The real bytes `be802100` in all 5 VS decode to exactly
`s_swappc_b64 s[0:1], s[0:1]`. So the RE'd contract is: **the driver preloads the fetch-shader pointer
into the user-SGPR pair s[0:1]; `s_swappc` saves the return PC back into s[0:1] and jumps to the fetch
shader; the fetch shader loads the vertex-buffer V# (`s_load_dwordx4`) and `buffer_load_format_* … idxen`
the per-vertex attributes into an agreed VGPR block (v[4:7] here); it returns via `s_setpc_b64 s[0:1]`;
the main VS then consumes those VGPRs.** Every one of the 5 VS opens `s_mov_b32 vcc_hi,imm` (the Orbis
prologue) → `s_swappc_b64 s[0:1],s[0:1]` → main body reading v4..v15.

**The mechanism — a leaf subroutine inlines exactly.** The fetch shader is a *leaf* (it only ever
returns — no nested call, no recursion, no branch back into the caller), so the call/return pair is
mathematically identical to **splicing the fetch body inline at the call site with its terminating
`s_setpc_b64` dropped**. That is the whole trick: `s_swappc_b64` needs **no** new interp op and **no**
new recompiler op. `crate::resolve_fetch_call(main, fetch)` (new `fetch_call.rs`) is a pure
`Decoded`-stream transform: find the single `s_swappc`, validate the SGPR-pair shape, confirm the fetch
body is a recognized fetch shader (`parse_fetch_shader`, which already existed), splice `fetch[..setpc]`
in place of the call, renumber `offset_dwords`, and return. After resolution the stream is plain
straight-line VS code (SMRD loads, idxen MUBUF fetches, then the main body) that the interp oracle and
the recompiler already handle **identically** — so the differential harness validates the whole thing
for free (a `fetch_*`-named callee corpus `.s` + an `inline_fetch_vs` caller, resolved before run /
recompile). Contract is strict-or-defer: zero calls → nothing to resolve; two calls, a non-SGPR fetch
pointer, or an unrecognized fetch body → a clean `FetchResolveError`, never a partial splice.

**Consequence — and the wall *behind* it.** With the call resolved, all 5 VS advance past `s_swappc`;
they then hit **one** shared next wall, `Smrd { op: 12 }`. RE'd via llvm-mc: the real bytes `c3000500` =
`[0x00,0x05,0x00,0xc3]` = **`s_buffer_load_dwordx16 s[0:15], s[4:7], 0x0`** (SMRD op 0x0C) — the 4×4
transform-matrix constant load the VS does right after the fetch call. This is just the wider sibling of
the AC #5 `s_buffer_load` path (count 16 instead of 4), so adding op 0x0C to the SMRD table flows through
the existing const-buffer SSBO emit/interp unchanged. With both, **all 5 target VS recompile end-to-end
to valid SPIR-V** (7→ the VS side clears; the other 2 VS, sh01/sh03, wall on the separate VCC-carry /
m0-read frontier, not the fetch call). Two lessons for this log: (1) a GCN *call* to a leaf subroutine is
a stream-inline, not an opcode — model the control-flow structure, not a fake "swappc executes X" op;
(2) clearing a structural wall routinely uncovers a mechanical one right behind it (here dwordx16) — RE
that one too before declaring the shader done, or the "cleared" wall just moves one instruction forward.

## Entry 10 — wiring the fetch-call VS into the *provider*: the fetch pointer and CB V# come from user-SGPRs, not the `.sb` (2026-07-15)

**Trigger.** Entry 9 landed the `resolve_fetch_call` stream transform + the `s_buffer_load_dwordx16`
op in `ps4-gcn`, so the 5 fetch-call VS recompile *in the offline harness*. But the live draw path
still deferred every Celeste draw at `NeedsGcn`: the `ps4-gnm` `GcnShaderProvider` had no way to
*reach* the fetch shader (it decoded the main VS, saw `s_swappc_b64`, and — before this — the
recompiler rejected it). Two seams had to be connected: (1) get the fetch-shader body to
`resolve_fetch_call` before recompiling; (2) bind the constant buffer the recompiler now declares.

**The hunt — where do the fetch pointer and the CB V# actually live?** The `.sb` container does *not*
carry either. The RE'd contract (Entry 9 + the corpus ABIs) says the **driver preloads them into VS
user-SGPRs**: the fetch-shader pointer into `s[0:1]` (the `s_swappc` address), and the constant-buffer
V# into `s[4:7]` — and critically, the CB V# is **inline in the SGPRs** (the `s_buffer_load` SBASE
names the four SGPRs *holding* the 128-bit V#), unlike the vertex path's `s[2:3]` which is a *pointer*
to a descriptor set. This is the architectural snag: the `ShaderProvider::resolve` trait has a bounded
memory seam but **no register access** (no `GpuState`) — the provider cannot read `s[0:1]` itself.

**The mechanism (the split — read registers where you have them, memory where the provider is).**
The fix threads the fetch address *down* rather than reaching *up*: `GpuState::derive_bound_shaders`
(which has the SH bank) reads `s[0:1]` and stamps it onto the `ShaderRef::GcnBinary`'s new
`GcnResources.fetch_addr`; the provider reads that field and pulls the fetch body through the *same
bounded seam* it already uses for the `.sb`. `read_fetch_code` **grows** its window (8→256 dwords)
until it captures the terminating `s_setpc_b64` — a fetch shader has no length declared at its pointer,
and growing (vs. shrinking a fixed large read) is what lets a fetch shader sitting near a mapping
boundary be read exactly. The constant buffer takes the mirror path: because its V# is *inline in
`s[4:7]`*, `derive_const_buffer` decodes it straight from the user-SGPR block (no memory read for the
descriptor) — the CB *bytes* are then pulled through the resource cache like any buffer. The recompiler
already declared the CB as a set0/bind2 `StorageBuffer` SSBO, so the backend just needed a **second**
VERTEX-stage `STORAGE_BUFFER` descriptor (distinct from the vertex-pull binding) in the one set-0
layout, plus a `BindConstBuffer` command — additive, no new descriptor-set.

**The discipline that carried over — defer the whole draw, never a half-bound pipeline.** Both new
paths copy the sampler path's rule exactly: if the fetch pointer is absent/unreadable, or the CB V# is
null, the *entire draw defers cleanly* (a `warn!`/`debug!`, `recompile_count` untouched) rather than
building a pipeline whose declared descriptor never gets a bind — that would be a Vulkan validation
error / GPU fault, not a clean skip. Every unresolvable case is a log that *pull-drives* the next ABI
correction (e.g. "fetch shader ... no pointer in s[0:1]" would say the fetch pointer is not where RE'd).

**Consequence.** The provider + executor + backend now carry a fetch-call VS end-to-end: main VS →
inline the fetch body → recompile the straight-line stream → declare the vertex-pull SSBO *and* the
constant-buffer SSBO → bind both. This is the last GPU-tier seam for the 5 fetch-call VS class; whether
Celeste's first pixel lands now depends on the *pixel* shaders recompiling (the separate VCC-carry/m0
PS frontier) and the driver preloading `s[0:1]`/`s[4:7]` as RE'd. Two lessons for this log: (1) when a
trait seam lacks the state a value needs (registers here), thread the value *onto the ref at the layer
that has the state* rather than widening the trait or adding a second global seam; (2) an SMRD `SBASE`
can name an **inline** resource (V# in SGPRs) or a **pointer** (descriptor-set base) — the const-buffer
V# is inline in `s[4:7]`, so it is decoded from registers, not chased through memory like the vertex
descriptor set.

## Entry 11 — the intermittent "executor SIGSEGV" is a RADV **ACO shader-compiler** crash on valid recompiled SPIR-V, not our seam (2026-07-16)

**Trigger.** Celeste boots fully and submits real ~4 MB command buffers; the executor reaches draws and
builds pipelines, but ~half the headless runs SIGSEGV (exit 139) with no clean guest-fault /
UnknownInstruction / missing-symbol diagnostic, right after the first `unhandled PM4 opcode 0x13
(IT_INDEX_BUFFER_SIZE)`. Debug-logged runs survived — the classic "it's a race" tell. The standing
hypothesis (doc-6 Entry 10 tail, task-113.4.1 notes) was a bad/racing register-derived V# dereferenced
through the *unbounded* `IdentityMem` on the new CB/vertex resolve path, or cross-thread provider-state
racing.

**Hunt.** A gdb `bt` on the fault (caught first try) put the crash squarely inside
`libvulkan_radeon.so`, called from `ash … create_graphics_pipelines` ← `ps4_gpu::backend::create_host_pipeline`
← `AshBackend::run_command_list` on the **display thread** — i.e. inside `vkCreateGraphicsPipelines`
compiling Celeste's recompiled VS/PS. Dumping the exact SPIR-V (env-gated hook) and running
`spirv-val` on both modules: **both are valid**. The decider: `RADV_DEBUG=llvm` (LLVM backend instead
of the default ACO) — the crash **vanished** (0 crashes across every run that reached the executor,
vs. a gdb-caught crash on the very first ACO run). So the SPIR-V is accepted by `spirv-val` *and* by
RADV's LLVM path; only RADV's **ACO** compiler segfaults on it, and the crash is heap/ASLR-sensitive
(hence "intermittent", hence "masked by debug logging").

**Mechanism.** Two independent facts, established by code audit, rule the standing hypothesis OUT:
(1) **The bounded seam is already watertight on the exec draw path.** Every register-derived descriptor
decode routes through `bounded_read()` (`derive_vertex_buffers`, `derive_texture_binding`) or reads the
V# straight from SGPRs (`derive_const_buffer`, no memory read at all), and every guest-*byte* read
(index/vertex/CB/texture uploads) routes through `BoundedMem::read_bytes_ranged`. A bad V# base is a
clean `Err` → the whole draw defers; it cannot reach an unbounded host deref. (2) **No cross-thread race
touches this crash.** All submit threads serialize through the single `static DRIVER: Mutex<GnmDriver>`
(the `record_submit` lock is held across `Executor::run`), and the submit→display hand-off
(`RunCommandList(cmds, signal)`) is *synchronous* — the submit thread blocks on `signal.recv()` while the
display thread compiles the pipeline. The pipeline's SPIR-V crosses as an immutable `Arc<[u32]>`. So the
crash is genuinely a RADV-internal ACO bug on a valid-but-degenerate module — the recompiled VS reads a
`Function` `uint` (`%14`) *before initializing it* (an undefined-value read, because this VS has no
vertex fetch to populate that VGPR), which is legal SPIR-V but is exactly the kind of input that stresses
a shader compiler's optimizer.

**Consequence.** The crash is a **GPU-driver-tier / recompiler-correctness wall**, not a bring-up-seam
bug — it is upstream of "first pixel" but downstream of everything task-113.4.1 owns. Two things landed
regardless, both strictly-safer: **(a)** an audit confirming the CB/vertex/texture V# resolve already
DEFERS cleanly through the bounded seam (no change needed — the hypothesis's premise was already false);
**(b)** the one residual *unbounded* guest access on the executor path — `write_label` (the EOP/EOS
label store, whose address is packet-derived and untrusted) — now range-validates the label slot against
the bounded VMA set before the identity store, so a bad label address is a clean no-op (a waiter stalls,
visibly) instead of an unbounded host over-write. **Workaround for the ACO crash while it stands:**
`RADV_DEBUG=llvm`. **Real fixes (out of this task's scope):** make the recompiler not emit
uninitialized-VGPR reads (task-56/GCN tier), and/or file the ACO crash upstream against a reduced module.

## Entry 12 — the "RADV present segfault" is a garbage `VkGraphicsPipeline` from a pipeline-layout stage_flags mismatch; the validation layer NAMES it (2026-07-16)

**Trigger.** After Entry 11's zero-init fix (task-134) + the PM4-opcode coverage (task-135), Celeste boots
Mono, submits GNM PM4, then host-SIGSEGVs (rc 139, SEGV_MAPERR) ~65 ms after
`sceGnmSubmitAndFlipCommandBuffers`. A coredump put the faulting RIP inside `/usr/lib/libvulkan_radeon.so`
(RADV) on the **present thread**, instr `cmpl $0x3b9ce510,0xb0(%rax)`, `%rax=0x38` — `0x3b9ce510` is a
RADV-internal sentinel. So RADV was validating a **garbage Vulkan handle we handed it**, not a guest fault.
(Two earlier guesses — a guest deref at struct `+0xb0`, and the `Graphics::*` dlsym misses — were both
wrong; the coredump→`.so` mapping is what corrected them.)

**The hunt — the coredump is opaque, the validation layer is not.** A garbage-handle → driver segfault
tells you nothing about *which* object. Re-running under **`VK_LAYER_KHRONOS_validation`** (Arch ships no
validation layer; it loaded fine from the Steam runtime's copy) caught the bad object ONE message before the
raw segfault: `VUID-VkGraphicsPipelineCreateInfo-layout-07988` — `pStages[1]` (FRAGMENT) uses descriptor
`[Set 0 Binding 2]` (STORAGE_BUFFER) but it was **not declared in the pipeline layout**.

**Mechanism.** Celeste's **pixel** shader does an `s_buffer_load`, so the GCN recompiler emits the set0/bind2
constant-buffer SSBO in the **FRAGMENT** SPIR-V (`recompile.rs` hardcodes `CONST_BUFFER_SET=0`/`BINDING=2`
for whichever stage loads constants). But the executor only harvested `const_buffers` from `vs_host.io`, and
the backend hardcoded that descriptor's `stage_flags = VERTEX`. So the FRAGMENT SSBO had no matching layout
binding → `vkCreateGraphicsPipelines` fails and returns a garbage/null `VkPipeline` → the draw/present uses
it → RADV dereferences it and segfaults. **The descriptor's stage_flags MUST match the stage whose SPIR-V
declares the binding.**

**Consequence (fix, task-139, merge 1beccfb).** `derive_draw_state` now harvests the const buffer from
*whichever* stage declares one (VS or PS), tracks the declaring `Stage`, reads its V# from that stage's
user-SGPR block, and defers cleanly if BOTH stages declare one (they collide on the single set0/bind2 slot —
strict-or-defer). `BackendCmd::CreatePipeline` carries `const_storage_fragment`; the backend sets the
descriptor `stage_flags` FRAGMENT vs VERTEX accordingly. RADV crash cleared (rc 139→124, process survives,
VUID-07988 count 0); frame is now a clean BLACK — no crash, no geometry — because the fix exposed the next
wall (the recompiled **VS** fails spirv-val *after specialization*, the task-128 stride spec-constant →
task-141). **Generalizable method (also a doc-4 taxonomy row): when a coredump lands inside the GPU driver
`.so` on a garbage handle, don't reverse the driver — re-run under the KHRONOS validation layer to get the
VUID + the named object/parameter, which points straight at the malformed `Create*Info` we built.**

## Entry 13 — Celeste programs GPU state through CP DMA: `IT_DMA_DATA` (with a memory→register variant) and `IT_INDEX_BUFFER_SIZE` bound the following draw (2026-07-16)

**Trigger.** After Mono boots and submits GNM PM4, the executor logs `unhandled PM4 opcode` for
`IT_DMA_DATA` (`0x50`) and `IT_INDEX_BUFFER_SIZE` (`0x13`) (plus a benign `IT_NOP` `0x10`), and the frame is
uniform WHITE — the following `IT_DRAW_INDEX_*` had no geometry to draw. Dispatch site
`crates/gnm/src/exec.rs` (the Type-3 match), opcode consts in `crates/gnm/src/pm4/opcodes.rs`. These two
packets carry the index/vertex-buffer setup for the draw, so dropping them empties the draw.

**Hunt.** Decode the two packets against GFX6 semantics. `IT_INDEX_BUFFER_SIZE` is a one-dword packet: its
payload is the index *count* that clamps the following `IT_DRAW_INDEX_*` (an upper bound on how many indices
that draw may consume). `IT_DMA_DATA` is the CP's DMA engine: its command dword carries `BYTE_COUNT[20:0]`
plus two address-space-select bits — `SAS[26]` (source) and `DAS[27]` (destination) — that pick, per side,
between *memory* space and *register* space. Reading Celeste's live stream: **every** `IT_DMA_DATA` it emits
is memory→**register** (`DAS=1`, destination ~`0x3022c` register offset, `BYTE_COUNT` 92..196) — Celeste uses
CP DMA to *program GPU registers*, not to move bulk memory.

**Mechanism.** `IT_INDEX_BUFFER_SIZE` → `IndexState.max_size`, which clamps the offset draw. `IT_DMA_DATA`
memory→memory is executed through the **bounded write seam** (`bounded_read` + `write_guest`, SMC-observed —
never a raw `IdentityMem` store), because the destination address is packet-derived and untrusted. The
memory→**register** / GDS variants — all of Celeste's — are decoded and cleanly **deferred** (no shadow
register file yet), and `IT_NOP` is silently skipped rather than logged.

**Consequence (task-135, merge 7d690da).** The unhandled-PM4 wall is gone; three opcodes decode, 197 gnm
tests. But the frame stayed WHITE and the process still SIGSEGV'd post-submit — proving the crash was a
*separate downstream* wall (the RADV garbage-handle of Entry 12, and later the mem/kernel aborts), not the
opcode gap. The deferred memory→register DMA is a **standing lead**: if Celeste programs its draw *context*
(vertex/const/RT state) via those register DMAs, the deferral leaves draw state incomplete — a hypothesis to
revisit if a future wall traces back to unset GPU registers. (It was *not* the cause of the Entry-12 garbage
handle; that was a stage-flags layout mismatch.)

## Entry 14 — PS4 direct memory is a *physical-offset* pool, and Mono frees sub-ranges *inside* live regions — so our unmap must be a no-op within the pool window (2026-07-16)

**Trigger.** Celeste's asset-streaming thread (Mono) nondeterministically aborts at `mono-mmap-orbis.c:219`
`g_assert(res==0)` mid-gameplay-load (≈1 run in a few), *after* a `sceKernelReleaseDirectMemory`. Our release
already returns 0 on every call (verified live) — so the assert is NOT reacting to our return value; it fires
inside Mono's own `mono_vfree` bookkeeping callback on the *success* path, downstream of a direct-memory
release. This is the memory-model half of getting Celeste's GPU workload to run at all.

**Hunt.** The PS4 direct-memory API is **physical-offset-based**, not VA-based: `AllocateDirectMemory`
returns a physical *offset* into the pool, `MapDirectMemory` maps that offset to a VA, and
`ReleaseDirectMemory(offset, len)` takes the *offset*. Our old model treated the release's `start` (an
offset) as a guest VA and blindly `munmap`'d it (`madvise(DONTNEED)` + dropped the VMA). Static disasm of the
free idiom (`call <import>; test eax,eax; js <cleanup>`) confirmed EAX==0 takes Mono into an indirect
`mono_vfree` tracking callback, whose invariant then trips. The real discovery: Mono **carves its own
sub-chunks** out of a larger direct-memory region and `munmap`s a VA sub-range in the **middle of a
still-live** region. Our `unmap` honoring that interior free zeroed/dropped pages Mono still referenced →
nondeterministic GC-heap corruption → the assert reads garbage.

**Mechanism / model (task-148).** Model direct memory as a faithful physical-offset **pool**:
`va = DIRECT_MEMORY_POOL_BASE (0x9_0000_0000) + offset` (5 GiB, consts in `core/kernel.rs`), a bump allocator
in `kernel/process.rs` whose offsets are **never reused**; allocate = reserve-only, map = `FIXED` map +
zero-fill fresh, release = bookkeeping-only. **Crux:** `VmBackend::unmap` now treats any unmap whose range
falls *inside the pool window* as a total no-op — no `madvise`, no VMA drop — so Mono's interior sub-range
frees can never corrupt the live region.

**Consequence.** Three clean 50–65 s Celeste runs with **zero** Mono aborts (was ~1/run); the asset thread
survives and the guest drives ~800–1366 GNM draws + ~157–254 flips + ~986 shader/texture binds per run. 436
workspace tests. The frame is still black — but now it is *purely* a GPU render wall (Entry 15), with the
memory and abort walls all cleared. Lesson: when a *foreign runtime's* allocator asserts on a success path,
suspect that our memory model desyncs *its* bookkeeping — a sub-allocator that frees interior ranges of a
region it owns demands that we honor the region as a whole, not each `munmap` literally.

## Entry 15 — the render trilogy that won Celeste's first frame: inline texture descriptors, vertex-fetch `dst_sel`, push-constant stride, and multi-pass videoout clear accounting (2026-07-16)

**Trigger.** After Entry 14, Celeste runs sustained gameplay and issues a *real* render workload (~800–1366
`DrawIndexAuto/Offset`, ~157–254 `SubmitAndFlip`, 986 binds per run) — yet the presented frame is uniform
BLACK, then (after the first fixes) uniform clear-color: real draws record and build real pipelines, but **not
one fragment survives**. Four independent GNM/recompiler mechanisms had to be reverse-engineered before a
single pixel landed.

**(a) Texture descriptor *provenance* — inline in user-SGPRs, not a memory descriptor (task-149).** A defer
histogram showed `tex_unresolved=953` (dominant) — every textured draw deferred. Root cause: Celeste's PS
`T#`/`S#` arrive **inline** in the user-SGPRs (`InlineVSharp{sgpr:0}` — the full 256-bit `T#`/`S#` sitting in
registers), *not* as a memory `SetPointer` descriptor. `derive_texture_binding` dereferenced the inline `T#`'s
first dword as a *pointer* → `VbufError::MemoryFault` → all textured draws deferred. Fix: dispatch on
`binding.source` and read `T#`/`S#` straight from the SGPRs (new `derive_texture_inline`) — mirroring the
const-buffer inline path from Entry 10. `tex_unresolved` 953→0, recorded draws 422→879.

**(b) Vertex-fetch `dst_sel` — the per-channel source-or-constant swizzle, and the NaN-`w` that clipped
everything (task-152).** Even with textures resolving and ~879 draws recording, **zero fragments**. The
recompiled SSBO vertex fetch read four *raw* dwords, but Celeste's position `V#` is `Format32_32_32` with
`dst_sel = [4,5,6,1]` — the `V#`'s `dst_sel` field selects, per output channel, a source component (`4..7`) or
a **constant** (`0`→`0.0`, `1`→`1.0`). Here the `w` channel is `SQ_SEL_1` = constant `1.0`; reading the raw
padding 4th dword instead yielded `gl_Position.w = NaN` → **every primitive clipped** → zero fragments. Fix:
the recompiled fetch honors `dst_sel` with per-channel nested `OpSelect` (`0`→`0.0`, `1`→`1.0`, `4..7`→source),
threaded as a VS push-constant (with a `DST_SEL_IDENTITY=0xFAC` passthrough that keeps the golden/oracle
green). Method that found it: a probe dumped viewport/MVP/vertex floats, disasm showed
`gl_Position.w = row3.(v4..v7)`, force-`w`=1.0 made geometry appear, and a runtime probe confirmed
`dst_sel=[4,5,6,1]`.

**(c) Vertex stride is a *push constant*, not a spec constant (task-140).** The recompiled VS's element
stride had been a `SpecId-0 OpSpecConstant`, but Vulkan spec constants bake at pipeline-**create** — which is
incompatible with the intent of keeping stride *out* of the `PipelineKey` (one pipeline serving all strides).
Two draws sharing shader+layout but differing in stride would then collide on the key and silently reuse the
first stride's pipeline. Fix: move stride to a **push constant** (VS push block `num_records@0`, `stride@4`),
so one pipeline serves all strides dynamically and stride legitimately stays out of the key; the
`OpSpecConstant` is removed (also closing the spirv-val-after-specialization concern). Celeste's SSBO draws
are all stride-16, so this was not the black-frame cause — but it had to be right before non-16 geometry could
render, and it settled the stride-key tension.

**(d) Videoout clear accounting across multiple draws *and* multiple submits (tasks 149, 152).** Celeste
issues up to **5 videoout draws per submit** AND splits its ~499 videoout draws across **many submits per
frame**. With a naive `loadOp=CLEAR` render pass, each draw (and each submit) cleared the previous one's
output. Fix: **CLEAR on the first videoout pass/submit of a frame, LOAD-and-accumulate thereafter** (a
per-frame latch), so draws accumulate into one framebuffer instead of clobbering. Two secondary fixes rode
along: a `PA_SC_SCREEN_SCISSOR` register off-by-one (`0x00D/E`→`0x00C/D`) and the cross-submit clear-clobber
latch.

**Consequence (merges 16a866c, 22da7ce; task-152 Done).** With all four in place, Celeste's loading-screen
progress bar + particle field **rasterize** — the first Celeste pixels, PNG-oracle confirmed (was black).
They are all WHITE (no color/texture yet) → the next wall was filed. Lesson for this log: "geometry submits
but nothing draws" is rarely one bug — it stacked a descriptor-provenance miss, a vertex-swizzle/constant
mis-fetch that produced NaN clip coords, and a per-frame clear-accounting error, each of which alone yields a
uniform frame.

## Entry 16 — tile-mode index 8 is GFX7 `ARRAY_LINEAR_ALIGNED` — plain row-major at a *padded pitch*, not a bank/pipe swizzle (2026-07-16)

**Trigger.** After Entry 15, Celeste's geometry rasterizes but every fragment is uniform WHITE — the colored
art (font atlas, UI atlas) lives in textures the executor DEFERRED whole (there was no detiler for their tile
mode), so the sampler read a fallback/uninitialized surface. A defer histogram put the macro-tiled deferrals
at 908/run, all of them the textured draws.

**Hunt.** Rather than reason about the swizzle, dump the raw swizzled texture bytes offline (`macro_raw.bin`)
and iterate a detiler against them until the alpha channel *straightens into readable glyphs*. The first
hypothesis was 2D macro bank/pipe swizzle (the shape we defer for `tile_idx>=9`). It was WRONG: a 1500-wide
32bpp atlas read tight-1500 sheared every row into a diagonal, and no bank/pipe inverse fixed it — the shear
was a constant per-row *slip*, the signature of a padded row pitch, not a 2D interleave.

**Mechanism.** **GFX7 tile-mode index 8 is `ARRAY_LINEAR_ALIGNED`: plain row-major whose row PITCH is padded
up to `align(width, 64)` texels** — for 32bpp the pipe-interleave is 256 B / 4 B = 64 texels, so a 1500-wide
atlas is *stored* at pitch 1536. Reading it at the tight width shears every row by the 36-texel pad. This is a
1D linear-with-padded-pitch layout, categorically distinct from the 2D macro (bank/pipe) swizzle of
`tile_idx>=9`.

**Consequence (task-153, merge 3d20861).** New `TileKind::LinearAligned` (tile-mode idx 8; `idx>=9` stays
`Macro2d`/deferred) with a pitch-strip detile — copy each row from the padded source pitch to the tight
destination pitch. Macro-tiled deferrals 908→0; the font/UI atlases detile into readable glyphs. The
oracle==upload byte-for-byte invariant (task-98/122) is preserved — the linear-aligned path only reshapes rows
it owns and leaves the identity/passthrough corpus untouched. `crates/core/src/tiling.rs`,
`crates/gnm/src/cache/tile.rs`. Lesson: a *diagonal* shear across rows is a padded-pitch tell, not a 2D
swizzle — a linear layout with an aligned pitch, decode it by copying rows at the padded stride, before
reaching for bank/pipe math.

## Entry 17 — Celeste's atlas VS fetches THREE interleaved vertex-buffer `V#` from one set — the recompiler bound one SSBO and packed only the first stream's params → attr2 UV read zero (2026-07-16)

**Trigger.** With textures detiling (Entry 16), geometry rasterizes but fragments are still black/white. A
forced-fragment-output probe (doc-5 case 21) bisected the failure to **attr2 (UV) reading exactly ZERO** in the
PS — the sampler had a valid texture but sampled at UV=0.

**Hunt.** Dump the fetch-shader `V#` layout the VS recovers from its user-SGPRs. The atlas VS pulls **three
distinct vertex-buffer `V#` — attr0 position, attr1 color, attr2 UV — from one descriptor set, interleaved
(same 24 B stride, different base address).** Then read what the recompiler+executor actually bound: **one
SSBO for *all* MUBUF fetches, and only the FIRST `VertexBuf`'s `num_records`/`stride`/`dst_sel` push-constants
packed.** attr2 therefore fetched against attr0's `num_records` → clamp → index 0 → zeros. A second bug rode
along: the recompiler hardcoded the MUBUF `soffset` + immediate offset to 0 (the all-offset-0 interp corpus
had never exercised a nonzero offset, so the oracle never caught it).

**Mechanism.** A GNM draw can bind several vertex-buffer `V#` in one set, each addressing a different stream at
its own base; the recompiled fetch must resolve *each* MUBUF against *its own* `V#` (base + `num_records` +
`stride` + `dst_sel` + `soffset`/imm-offset), not fold them all onto stream 0.

**Consequence (task-153, merge 8ec13fc).** One SSBO binding + one push-constant group *per distinct vertex
`V#` stream* (bindings 0/3/4; the PS sampler keeps binding 1, the const buffer keeps binding 2), with the MUBUF
offset threaded through. Two differential goldens with teeth — `offset_fetch_vs` (nonzero offset) and
`inline_multi_fetch_vs` (multiple interleaved streams) — lock the fix so the all-zero-offset corpus gap can't
reopen. The forced-UV probe then showed a clean gradient (proof the UVs now fetch).
`crates/gcn/src/recompile.rs`, `crates/gnm/src/exec.rs`, `crates/gpu/src/backend.rs`. Lesson: one descriptor
set ≠ one vertex stream — enumerate the `V#` the fetch shader recovers and give each its own SSBO + params, and
grow the differential corpus to include a nonzero offset (an all-zero corpus silently blesses a hardcoded 0).

## Entry 18 — Celeste presents by DOUBLE-BUFFERED DIRECT SCANOUT, not render-to-texture — and three videoout bugs (unread buffer list, a no-op *writer* stub, a dropped flip index) broke it (2026-07-17)

**Trigger.** The scene renders into "offscreen RTs" (`0x982c48000`, `0x9b00e0000`) that nothing composites
back to the screen — `rt_hit=0`, the RT never flips, and the videoout composite samples a garbage/uninitialized
inline `T#`. The presented frame is white-on-black.

**Hunt.** The first hypothesis — render-to-texture with a GPU resolve→sample compositing the RT — was
DISPROVEN by the PM4 trace: **no resolve/copy packet exists; every `IT_DMA_DATA` is memory→register** (Entry
13), so nothing on the GPU moves that surface to the screen. So the compositing must be on the *videoout* side.
Trace `sceVideoOutRegisterBuffers` + the flip and read the SDK header
(`data/oo_sdk/include/orbis/_types/video.h`): the attribute struct carries `width@+12`, `height@+16`.

**Mechanism.** **Celeste uses double-buffered DIRECT SCANOUT, not render-to-texture.** It registers TWO
scanout buffers (`list[0]=0x981670000`, `list[1]=0x982c48000` — the "offscreen RT" IS scanout buffer #1),
renders the scene *directly* into one, and flips a chosen `buf_idx` via
`sceGnmSubmitAndFlipCommandBuffers`. Three bugs broke this:
- **(a) unread buffer list.** `video_out_register_buffers` read only `list[0]` and ignored the count → buffer
  #1 was misclassified as a private offscreen RT (hence `rt_hit=0`).
- **(b) a no-op *writer* stub with the wrong signature.** `sceVideoOutSetBufferAttribute` was stubbed as a
  3-arg no-op — but it is a **7-arg WRITER that fills `*attribute`**. Because we no-op'd it, the guest's
  attribute struct stayed garbage, so `read_videoout_attr` (whose offsets `width@+12`/`height@+16` were
  *correct all along*) read a **degenerate 1×3 display buffer** — the white-on-black cause.
- **(c) a dropped flip index.** `AndFlip` dropped the `buf_idx` stack argument and always presented index 0.

**Consequence (task-153, merge ad7bb19).** Register *all* buffers (honor the count); implement
`sceVideoOutSetBufferAttribute` as the 7-arg writer at the SDK offsets; thread `buf_idx` through the flip so
the existing Videoout path routes the scene to screen. White-on-black is gone; colored content reaches the
scanout buffer. `crates/libs/src/libscevideoout/mod.rs`, `crates/libs/src/libscegnmdriver/submit.rs`,
`crates/gnm/src/exec.rs`, `crates/gpu/src/lib.rs`. **Lesson (also a doc-5 case): check the WRITER, not just
the reader.** `read_videoout_attr` looked suspect because it produced 1×3, but its offsets were right — the
struct it read was garbage because the *writer* that should have filled it was a no-op stub. When a struct
reads as garbage, audit whoever was supposed to *write* it before you second-guess the reader.

## Entry 19 — the Vulkan pipeline hardcoded `blend_enable=0`, so Celeste's premultiplied-alpha layers overwrote the scene to RGB≈0 (alpha-only) (2026-07-17)

**Trigger.** After the scanout fix (Entry 18), the scene reaches `texture_image` but presents colorless — a
mid-run dump reads RGB `0/0/0` with ALPHA mean 218: the draws run and cover the screen, but produce RGB=0.

**Hunt.** Rule out the shaders and the key first. The recompiled fragment shaders are correct (`pkrtz`→`exp
mrt0` writes full RGBA), and `derive_blend` correctly threads a `BlendKey` into the `PipelineKey` (4/5 pipelines
report `blend.enable=true`). So the blend *state* is derived correctly — yet blending doesn't happen. The gap
is between the key and the Vulkan pipeline object.

**Mechanism.** **`create_host_pipeline` HARDCODED `blend_enable=0` and wasn't even passed the `BlendKey`**, so
every draw *fully overwrote* its pixels regardless of the derived state. Celeste composites premultiplied-alpha
layers; the final fullscreen premult-black-over layer therefore *overwrote* the accumulated scene → RGB≈0,
alpha only.

**Consequence (task-154, merge dad45a4).** Translate `CB_BLEND0_CONTROL` into the Vulkan blend state, gated on
enable bit 30: COLOR `SRC[4:0]` / `COMB[7:5]` / `DST[8:12]`, and when `SEPARATE_ALPHA[29]` is set the ALPHA
`SRC`/`COMB`/`DST` fields, mapping the GFX6 blend-factor/op enums to `vk::BlendFactor`/`vk::BlendOp`. Anchors
that pinned the enum mapping: `0x45010501` = premultiplied-over (`SRC=ONE`, `DST=ONE_MINUS_SRC_ALPHA`),
`0x41040104` = additive. With blending live the "Matt Makes Games Inc. presents" splash composites in correct
warm pink/orange — **Celeste's first colored frame** (PNG-oracle confirmed). `crates/gpu/src/backend.rs`.
Lesson: a correctly-derived render-state *key* is worthless if the pipeline builder doesn't consume it — when
state is derived right but has no visible effect, check that the value actually reaches the `Create*Info`, not
a hardcoded default.

## Entry 20 — instant EOP-fence completion made the guest's gnmx skip per-frame texture binds → Celeste logo rendered as a white bar (2026-07-18)

**Trigger.** After the white-dummy/blend fixes, Celeste's steady-state splash still renders the logo as a solid
WHITE BAR (atlas missing): the per-frame texture binds collapse `3,3,0,0,0…` — the three content draws re-emit
their 8-dword T# (`SET_SH_REG 0x2c0c`) + S# (`0x2c14`) only on frames 0 and 1, then drop them forever. Real PS4
ground truth (`data/celeste-real-dcb/`) re-emits all three EVERY frame. Four prior phases ruled out the CE, DMA,
arena staleness, register persistence, value-dedup, GC/data-loss (atlas descriptors stay resident on frame 2 —
collapse is emission-only), and the virtual clock (falsified twice, byte-identical). Decompiling MonoGame's
`TextureCollection` showed a pure managed `_dirty` bitmask reset all-dirty every `Present` — so the divergence
had to be in the NATIVE gnmx layer (stripped AOT), not the managed gate. Decisive correlation: the collapse
coincides exactly with the first REUSE of the double-buffered command context.

**Hunt (falsifiable experiment, task-157 Phase 5).** Instrumented the run: the guest registers ONE GPU-completion
event (`sceGnmAddEqEvent` type=64 EOP, id=0) and calls `sceKernelWaitEqueue` every frame, always returning
`triggered=Some` because our synchronous executor signals completion at submit-done. Env-gated an emulated
completion LATENCY (`UNEMUPS4_GPU_LATENCY`, `UNEMUPS4_GPU_LATENCY_SUBMITS`) and swept it, re-decoding per-frame
bind counts. **Result 1:** deferring the EQUEUE completion signal — even forcing the wait to observe "GPU not
done" on many frames — changed NOTHING (`3,3,0,0,0` byte-identical). **Result 2:** deferring the EOP MEMORY-FENCE
label write (the `IT_EVENT_WRITE_EOP` label, one per flip DCB, ping-ponging between two per-context addresses
`0x902046a08`/`0x902c46a10`) shifted the collapse out by exactly the latency (`collapse frame = depth + 2`).
**Result 3 (decisive):** withholding the fence "done" entirely made gnmx re-emit all three binds EVERY frame
(`3,3,3,…` indefinitely) — matching real HW — with NO deadlock, and the PNG oracle showed the fully TEXTURED
"Matt Makes Games Inc. / presents" splash (gradient, font atlas, bokeh particles) instead of the white bar.

**Mechanism.** The Sony gnmx SDK recycles its double-buffered command contexts by CPU-polling the EOP memory
fence ("is the GPU done with this buffer, may I refill it WITHOUT re-initializing it?"). Our executor is
SYNCHRONOUS (doc-2 §C2): `Executor::run` writes the fence value inline, before the submit HLE call even returns,
so gnmx always sees the buffer as already free and takes a fast recycle path that SKIPS re-emitting the buffer's
per-draw state — including the atlas T#/S# binds. On real hardware the EOP write is asynchronous (the GPU is
perpetually ~1–2 frames behind the CPU), so gnmx sees the reused buffer as still in flight and re-records the
FULL state every frame. No fixed finite latency reproduces this: at steady 60 Hz the reused buffer's fence is
always old enough to read "done" by the recycle check, so latency only shifts the collapse out by `depth`
frames; only leaving the fence perpetually in-flight keeps gnmx on its always-correct re-record path.

**Consequence (task-157, fix in `crates/gnm/src/exec.rs`).** Split the label emit from the store: `emit_label`
now DEFAULTS to pipelined completion — it does NOT surface the EOP/EOS memory fence synchronously (the guest's
real frame-sync GPU-completion signal is the EQUEUE event, which we still fire on submit-done via
`equeue::signal_gpu_completion`; the raw memory label is, for guest correctness, only gnmx's buffer-recycle
hint). `UNEMUPS4_GPU_EOP_SYNC=1` restores the pre-fix inline write for A/B and for any title that CPU-polls this
memory label as its ONLY completion signal. Verified bidirectionally: default → `3,3,3,…` + textured splash;
`UNEMUPS4_GPU_EOP_SYNC=1` → `3,3,0,0,0` + white bar. **Lesson:** a SYNCHRONOUS software GPU can mislead guest
middleware that polls a memory fence to time buffer recycling — reporting completion "too early" is as wrong as
reporting it late. When guest emission collapses on buffer REUSE, suspect the completion signal the reuse path
polls, and prefer surfacing completion through the primitive the guest actually blocks on (the equeue), not an
inline memory-fence write the guest only uses as an optimization hint.

## Entry 21 — a suspected animation "bug" was FAITHFUL to hardware; capturing real-HW referenced-BUFFER CONTENT (not just the DCB) is the disambiguating oracle (task-170/172/173)

**Trigger.** After the EOP fix (Entry 20), Celeste's studio splash rendered textured but the intro animation
appeared to "cofa się" — the background/snow scrolled then snapped back every ~27 flips, and the text seemed to
replay. Suspected our bug.

**Hunt.** Ruled out, in order: (a) the guest clock — `UNEMUPS4_CLOCKLOG` showed every guest time source strictly
monotonic, exactly one FRAME_NS/flip, no periodic discontinuity (x86jit rdtsc is a constant, so the animation is
virtual-clock-driven, not rdtsc); (b) structural scene-replay — a per-flip DCB draw-signature timeline plus a
VS/PS shader-program fingerprint (`SPI_SHADER_PGM_LO` low-12, load-base-stable) proved our recurring "5-draw"
block is a DIFFERENT scene (steady 6f8/6f7), not the intro overlay (e59/47f) re-entering; the intro plays exactly
once, like real HW; (c) boot-time clamping — any cumulative BOOT-phase virtual-time ceiling deadlocks boot (the
boot spin-wait needs >2 s of virtual time), so the intro-ease compression is not clamp-fixable. The DCB alone
could not settle it, because the animation lives entirely in REFERENCED dynamic vertex/uniform buffers (the DCB
is byte-identical every other frame; zero inline animation data).

**Mechanism (the decisive tool).** Extended the GoldHEN scraper (task-168) to capture the CONTENT of those
referenced buffers off real hardware, not just the DCB: the plugin parses each flip's live DCB on-device (shadows
VS/PS user-data, reads inline V#s + follows user-data pointers), guards every read with `sceKernelVirtualQuery`,
and streams each `[base, span]` with a new `KIND_VBUF` wire kind. A role key `(vs-lo12, ps-lo12, stride,
num_records, span)` aligns real-HW dumps to our `UNEMUPS4_DUMP_VBUF` dumps despite differing load bases. Decoding
the real transform constant-buffers frame-by-frame showed **real HW's own steady-scene transform CB is a ~28-flip
ramp-and-reset SAWTOOTH** (float ramps then hard-resets) — the exact shape/period of our "loop." Our UI-quad
geometry is byte-identical to real HW. **Verdict: the animation content is FAITHFUL; the ~27-flip loop is
by-design Celeste, present on real hardware too — not our bug.**

**Consequence.** The residual visible artifacts are elsewhere: (1) the intro-zoom is now stable (Entry-20 fix +
faithful content); (2) a spun-out real bug — `decode_s_sharp` ignored the S# CLAMP_X/Y wrap field and every
sampler hardcoded REPEAT, so a CLAMP texture (real Celeste's backdrop) tiled; fixed in task-173 by decoding
word0[2:0]/[5:3] and honoring per-axis address modes (Mesa `V_008F30_SQ_TEX_WRAP` codes); (3) the title-screen
atlas-splatter + flicker is a separate RT/compositing gap (task-171). **Lesson:** when a retail title "looks
wrong," first decide whether the guest is even producing different output than real HW — a captured PM4 DCB shows
the commands but NOT the referenced buffer contents where dynamic animation lives; capturing that content off
real hardware (role-keyed so load bases don't matter) turns "is this our bug or faithful behavior?" from a guess
into a measurement, and here it saved us from "fixing" correct emulation.

## Entry 22 — the warm-palette bug was TWO independent bugs that masked each other, and the "correct" reference scene was never correct (task-175)

**Trigger.** Celeste's title screen rendered in a warm palette (pink mountain, gold/purple background, yellow
logo) where the correct output is cool (blue/purple mountain, navy background, cyan logo). Swapping R↔B on the
correct colours reproduced our palette exactly, so the task was written as a channel-order bug — with the crucial
qualifier that the studio splash "renders correctly", which made a global fix look impossible and sent an earlier
session hunting for a per-scene signal.

**Hunt.** The per-scene signal does not exist. `CB_COLOR0_INFO` probed live is a single value for the entire run
— `0x00008828` → FORMAT `0xA`, NUMBER_TYPE `0` (`NUMBER_UNORM`), COMP_SWAP `1` (ALT/BGRA). Splash and title share
the target, so they cannot differ. The premise was false: **the splash was never correct either** — it carries the
same warm cast, and was mis-scored because a warm gradient reads as a plausible "gold gradient" when you do not
have the reference side by side. Offline correction of a captured flip settled the rest: R↔B alone yields correct
hues but a milky, washed-out background (this is the "teal splash" an earlier attempt produced and then
self-refuted); removing one sRGB encode alone leaves the hues wrong. Only both together reproduce the reference.

**Mechanism.** Two independent defects, each invisible while the other stood. (1) **Gamma.** The guest videoout
colour buffer is UNORM, but we rendered into an `R8G8B8A8_SRGB` image. Celeste composites in gamma space — as a
UNORM CB does on real hardware — so its fragment values are already gamma-encoded and the `_SRGB` attachment
encoded them a second time on store. (2) **Channel order.** `swap_rb` describes the byte order of *guest memory*,
so it is owed only when present sources pixels from the guest framebuffer. An embedded GNM draw writes
shader-space `(r,g,b,a)` straight into an RGBA image; on real hardware the guest's COMP_SWAP and its scanout
pixelFormat describe the same buffer and compose to identity, so no swap is ever owed to embedded content.
Celeste is `embedded_drawn = true` from ~flip 300, so it took an unmatched R↔B flip every frame. Fix: videoout
image → `R8G8B8A8_UNORM`, `swap_rb = !embedded_drawn && scanout_swap_rb(...)`, and the present shader now
*decodes* sRGB so the `_SRGB` swapchain's encode-on-store cancels — guest bytes pass through untouched end to end.

**Consequence.** Both scenes render correct. Still open: `create_rt_target` hardcodes `EMBEDDED_TARGET_FORMAT` for
offscreen RT render passes while RT images come from `vk_color_format`, so for Celeste's COMP_SWAP=ALT
`B8G8R8A8_UNORM` targets the render-pass format still differs from the image format in channel order. The current
driver tolerates it; MoltenVK/Metal may not. **Lesson:** when two format defects sit on the same path, every
single-variable experiment refutes itself and the evidence reads as "neither hypothesis is right" — vary them
together before discarding either. And score EVERY scene against the reference: an artifact that happens to look
plausible ("gold gradient") gets banked as a fixed point, and a false fixed point is worse than no oracle at all,
because it makes the true global fix look structurally impossible.

## Entry 23 — IT_ACQUIRE_MEM: coherency size/base count 256-BYTE UNITS, and the barrier Celeste asks for is one we satisfy by a different mechanism (task-178 follow-up)

**Trigger.** `[GNM] unhandled PM4 opcode 0x58 (IT_ACQUIRE_MEM)` — 582 log lines in a 90 s Celeste run, and the
last unhandled opcode left in the title's command stream.

**Hunt.** Mesa is the spec reference (we do not crib emulator sources). `src/amd/common/ac_cmdbuf_cp.c:433-439`
emits `PKT3(PKT3_ACQUIRE_MEM, 5, 0)` with every dword commented by register, giving the 6-dword body
`[coher_cntl, coher_size, coher_size_hi, coher_base, coher_base_hi, poll_interval]`; `ac_parse_ib.c:382-393` dumps
the older `PKT3_SURFACE_SYNC` as the same packet minus the two `_HI` dwords. **The field names carry the detail
that matters: `S_030230_COHER_SIZE_HI_256B` / `S_0301E4_COHER_BASE_HI_256B` — size and base count 256-byte units,
not bytes**, and the `_HI` halves are 8 bits wide, so a 40-bit unit index yields a 48-bit byte address. Guessing
bytes here silently produces ranges 256× too small.

**Mechanism (and the confirmation).** Scanning the real-PS4 oracle (task-168) found 13040 ACQUIRE_MEM packets in
exactly 4 distinct bodies: one whole-memory acquire per frame as the DCB preamble (`coher_size` saturated), plus
three bounded ones all with `coher_cntl=0x82c40040` = `CB0_DEST_BASE_ENA | TC_WB_ACTION_ENA | TCL1_ACTION_ENA |
TC_ACTION_ENA | CB_ACTION_ENA` — a colour-buffer → texture barrier. The 256-byte shift is confirmed by the
arithmetic, not by trust: the decoded sizes are *exactly* 1920×1088×4 and 1024×576×4. Our own submit path emits
the same cntl and the same three sizes in the same 2:1:1 ratio (different bases — different allocator), which
cross-validates the decode from both ends.

**Consequence.** The packet is decoded and consumed (582 → 0 unhandled lines), and bounded acquires are wired to
`ResourceCache::invalidate_range`. **That wiring is deliberately inert today, and the zero is the correct result,
not a gap:** every range Celeste acquires over is a render target, and `invalidate_range` exempts `is_rt` entries
by design — `dirty` there means "re-upload from guest memory", which for an RT we rendered GPU-side would
overwrite our own output with stale guest bytes. Measured: 2368 bounded acquires, 0 entries marked. What Celeste
asks for (RT → texture) we already satisfy through the RT-as-texture path, where the image never leaves the GPU;
the wiring covers the case that path does not — an acquire over a plain buffer (a compute-written vertex/index
range), the one situation the guest-CPU dirty drain provably cannot observe because the writer is the GPU.
**Lesson, and the thing to remember when a bug lands here:** we currently satisfy the RT→texture barrier
*incidentally*, without honouring the packet — there is no GPU↔GPU synchronization at this point at all. If a
render-to-texture artifact ever appears (a texture one frame stale, tearing within a frame), this packet is the
first suspect and the natural place to emit a `vkCmdPipelineBarrier`. Also: an opcode whose handler measurably
does nothing is worth landing anyway when the packet must be *consumed* for the parser to stay in sync — but say
so in the code, or the next reader will mistake the wiring for a working fix.

## Entry 24 — VS→PS attribute routing is programmable (`SPI_PS_INPUT_CNTL`), and assuming identity silently feeds a shader the wrong input (task-179)

**Trigger.** Celeste's main menu rendered the menu text over a flat gradient — the 3D
mountain behind it was gone. RenderDoc showed the mountain present and correct in an early
colour pass, so it was being drawn and then lost.

**Hunt.** Long, and mostly down the wrong roads. The visible symptom was a composite that
looked like an opaque replace, so the search started at blend state and worked outward:
`CB_BLEND0_CONTROL`, `CB_TARGET_MASK`, `CB_SHADER_MASK`, `CB_COLOR_CONTROL`, render-target
clears, pass ordering, barriers, image binding, vertex UVs and geometry, the const-buffer
reads, the export lowering, and the generated SPIR-V. Every one of them checked out. The
turn came from two things, neither of which was more instrumentation: env-gated knobs the
maintainer could flip one at a time and judge by eye, and then a single RenderDoc look at
the failing blur draw comparing its **input** against its **output**. The input was the
correct scene texture; the output was a constant colour.

**Mechanism.** A correctly-bound texture that samples to a constant means the UV reaching
the sampler is constant. The UVs in the vertex buffer were already measured correct, so the
break was between the vertex shader and the pixel shader. On GCN,
`SPI_PS_INPUT_CNTL_n.OFFSET` (bits [4:0] of `R_028644 + n`) selects **which VS export
parameter feeds PS attribute slot `n`**. The mapping is programmable and it is not the
identity. The register appeared nowhere in the tree; the recompiler decorated a PS
interpolant with its own attribute index. Celeste's two blur draws program `OFFSET = 1` on
slot 0 and read their UV from `attr0`, so the shader received VS parameter 0 — the vertex
colour, a constant `0xFFFFFFFF` across the quad. Constant UV → one texel sampled → constant
output → a bloom target with no scene in it, which then wiped the frame when composited.

**Consequence.** PS interpolants are decorated with the location the register names.
`recompile()` stays as an identity wrapper over `recompile_with()`, because the
differential harness diffs against `interp`, which has no notion of routing. Two subtleties
worth carrying forward:

- **Aliasing is real.** Under a non-identity map two distinct attribute slots can resolve
  to the same location (unused slots commonly read `OFFSET = 0`), and two Input variables
  sharing a `Location` is invalid SPIR-V. Interpolant bookkeeping must key on the resolved
  location, not the attribute index.
- **The routing belongs in every shader cache key.** The same PS binary under different
  `SPI_PS_INPUT_CNTL` is a different module. Keying only on the `.sb` bytes would cache the
  first-seen variant and hand it to the second, reproducing this bug in a form that no
  longer correlates with any one draw.

**The generalisable lesson.** This was the FOURTH register in a single investigation that
exists in hardware, changes results, and was never read — after `CB_TARGET_MASK`,
`CB_SHADER_MASK` and `CB_COLOR_CONTROL`. The first three were harmless here and the fourth
was the bug, which is exactly why finding them one wall at a time is expensive: three
harmless misses train you to stop suspecting the category. Two of them were even *named in
the comments* of the derivation that ignored them, and one we emit ourselves in our own PM4
and never consume. task-183 tracks a deliberate audit of the rest rather than waiting for
the next wall to surface one.

The debugging-method lesson is separate and just as reusable: the two measurements that
misled this investigation were both readbacks, both wrong, and both wrong in the direction
that flattered the hypothesis being tested. A knob the maintainer can flip produces a
judgement from outside the model's own reasoning, and that is what broke the deadlock.

## Entry 25 — `v0` IS the vertex index: a VS that reads it directly got zero, so every guest full-screen fill drew nothing (2026-07-20, task-184)

**Trigger.** Celeste's menu rendered the 3D mountain uniformly soft, as if the blurred copy
had replaced the sharp scene rather than adding a glow to it. Reading the dumped render
targets found the bloom target's alpha pinned at min 255 / max 255 across the whole
surface, against a predicted radial ramp of ~16 at centre and ~155 at the corners — the
target was saturating, not attenuating.

**Hunt.** The bloom targets are cleared by the guest with a full-screen *fill draw*, not by
a render-pass clear: their pass is `LOAD`, so if the fill writes nothing, nothing else
zeroes them, and under the guest's premultiplied-over blend the alpha accumulates frame
over frame to saturation. The fill's VS is the standard index-derived full-screen triangle
— `x = (idx & 1) * 2 - 1`, `y = (idx & ~1) - 1`. Disassembling the recompiled module (NOT
the GCN disassembly, which looked entirely plausible) showed the tell:
`OpEntryPoint Vertex %11 "main" %gl_Position %70` — no `gl_VertexIndex` in the interface —
and `%15 = OpVariable %_ptr_Function_uint Function %uint_0`, the index as a local
initialised to zero. With the index stuck at 0 all three vertices collapse onto
`(-1,-1,0,1)`: a zero-area triangle that rasterises nothing.

**Mechanism.** On GCN the launch ABI hands a vertex shader its vertex index in `v0`. Our
recompiler models VGPRs as zero-initialised Function variables and had a tracker
(`vertex_index_regs`) for "VGPRs still carrying the launch index" — but consulted it only
at the `idxen` MUBUF vertex fetch. Every corpus VS reached the index through a fetch, so
the gap never showed. A VS that reads `v0` as a plain ALU source has no fetch to intercept,
and read the zero slot. The interpreter did NOT have this gap — it seeds
`vgprs[0][lane] = first_vertex + lane` — so the two engines silently disagreed, and the
differential harness could not see it because no corpus shader had that shape.

**Consequence.** The tracker is now consulted at the register-read chokepoints
(`load_reg_u32` / `load_reg_f32`), so any read of a still-tracked reg resolves to
`gl_VertexIndex`. The MUBUF fetch destination now untracks, closing the
`buffer_load_format_x v0, v0, …` case the widened read path would otherwise have kept
stale. Both draw modes are correct against hardware: `DrawIndexAuto` → `vkCmdDraw`, where
`gl_VertexIndex` is sequential; `DrawIndex2` → `vkCmdDrawIndexed`, where `gl_VertexIndex`
is index-buffer driven — which is exactly what GCN's VGT delivers in `v0` for an indexed
draw. The residual constraint is against the *interp oracle*, which has no index buffer.

**The generalisable lesson.** A mechanism that exists for one path and is only *consulted*
on that path is not implemented — it is a special case wearing a general name. The tracker
had the right model of the launch ABI and the right comment describing it; what was missing
was consulting it everywhere a register is read. When a shape is absent from the corpus,
the differential harness is not evidence of agreement — it is silence. The corpus gained
`index_tri_vs` (self-authored, no vertex buffer, no fetch shader) specifically so this
shape is represented, and the assertion is at the SPIR-V *module* level, because this bug
was invisible in the disassembly and visible only in the entry-point interface.

## 2026-07-20 — the fill draw needed TWO more fixes: an in-place register read, and RECTLIST

**Trigger.** The launch-ABI fix above landed, reached the GPU (verified by diffing two
RenderDoc captures: the fill VS module grew 1524 → 1576 bytes at `vkCreateShaderModule`)
— and the picture did not change at all. Celeste's bloom targets still came out with a
flat alpha of 1.0, measured directly from the snapshot's own render-target PNGs (min 255,
max 255 across the whole surface, both targets).

**Hunt.** The handed-down hypothesis was a descriptor mix-up: the VERTEX constant buffer
landing at set0/binding 6 where the PIXEL one belongs, since the vertex CB's dword 2 is
exactly `0.0` and `const.z = 0` is on its own sufficient for the observed uniform replace.
It was refuted from the pictures, not from the code. RT_C fits a five-tap vertical blur of
RT_B at ±2 and ±4 rows to an RMSE of 1.6/255 — the kernel `const.y = 1/540` produces.
Under the vertex CB, `const.y` would be `0` and `const.x` would be `1/480`: a horizontal
blur at ±4/±8, RMSE 3.8. An all-zero SSBO gives the identity, RMSE 3.3. So dwords 0 and 1
of the PIXEL buffer demonstrably reach binding 6, and the whole constant-delivery line is
dead for the third and last time.

That measurement also killed a tempting shortcut: RT_C's RGB *cannot* distinguish "clear
works and the attenuation is lost" from "clear fails and the attenuation is applied".
Under premultiplied-over into a target that is never zeroed, the fixed point is
`dst = f·blur/f = blur` — exactly a correct blur, no saturation. Both hypotheses predict
the same RGB and the same alpha of 1.

**Mechanism (two independent defects, BOTH required).**

1. *The launch index resolved on the first read only.* The fill VS is
   `v_and_b32 v1, 1, v0` then `v_and_b32 v0, -2, v0`. Every ALU emitter untracks its
   destination as a launch-index carrier BEFORE evaluating its source operands, and a
   tracked register lives only in the tracker — its Function slot still holds the zero
   initializer, because the index is materialized on demand at each read. So the in-place
   second read got zero. `gl_VertexIndex` was correctly declared, correctly in the
   entry-point interface, correctly loaded — and the Y coordinate of all three vertices
   was still pinned to -1. A zero-area triangle, again. The corpus shader added for the
   previous fix (`index_tri_vs`) writes a *different* VGPR on its second read, so the
   differential harness was silent about exactly the shape that shipped.

2. *`VGT_PRIMITIVE_TYPE` was never modelled.* The register (uconfig `0xC242`) is `0x11`
   = `DI_PT_RECTLIST` for precisely the five fill draws in the frame and `0x04` =
   `DI_PT_TRILIST` for every other draw — visible in the snapshot's per-draw register
   delta, invisible in `registers.json`, which is end-of-frame. A GCN rect list takes
   THREE vertices per RECTANGLE: `p0`, `p1`, `p2` name three corners and the hardware
   synthesizes the fourth as `p2 + p1 - p0`. Celeste's fill VS emits `(-1,-1)`, `(1,-1)`,
   `(-1,1)` — three corners of the screen. Rasterized as a triangle list, that is a
   half-screen triangle; as a rect list, the full screen. So even with (1) fixed, the
   clears would have covered half their target.

**Consequence.** `untrack_vertex_index` now SPILLS the builtin into the register slot
before untracking, so the generic register path is correct at every later read including
the instruction doing the clobbering; it is used at all eleven untrack sites.
`PipelineKey` gained a `topology` field derived from `VGT_PRIMITIVE_TYPE`; a rect list
builds a triangle-STRIP pipeline and a non-indexed rect draw is issued with FOUR vertices,
whose two strip triangles tile the same parallelogram. The fourth vertex comes from the
stream at index 3 rather than from the hardware's corner synthesis — they coincide for the
index-derived full-screen-fill idiom and diverge for a vertex-fetched rect list; that
approximation is documented on the enum. Every other primitive type keeps the historical
triangle-list behaviour, so modelling the register cannot regress a title that never sets
it. The snapshot now records the per-draw topology.

**The generalisable lesson — two of them.**

*A test that shares a name with the bug is not a test for the bug.* `index_tri_vs` was
added to represent "a VS that reads `v0` directly", passed, and shipped a shader that
still rendered nothing. The shape that mattered was narrower: a read whose instruction
also WRITES that register. When you fix an ordering bug, the regression test has to have
the ordering in it. `index_tri_inplace_vs` is byte-identical to Celeste's fill VS body.

*Render-target pictures are quantitative evidence.* Three hypotheses about what reaches a
shader were settled by fitting candidate blur kernels to the dumped PNGs and comparing
RMSE — no probes, no re-run, no maintainer time. The picture is a witness about the
descriptor; a descriptor dump would not have been a witness about the picture. But the
same pictures were ambiguous about the clear, and knowing WHICH question the image can
answer is the whole skill: the fixed point of an accumulating premultiplied blend is
indistinguishable from a correct one-shot blend.

## Entry 26 — `VGT_PRIMITIVE_TYPE` and a tracker that reads zero on in-place update: why the fixed fill still drew nothing (2026-07-20, task-184)

**Trigger.** Entry 25's fix — seeding `gl_VertexIndex` for a VS that reads `v0` directly — landed and
demonstrably reached the GPU, yet Celeste's menu was unchanged. Verified by differencing two RenderDoc
captures taken before and after: the modules handed to `vkCreateShaderModule` grew 1524→1576 bytes
(381→394 words) and 4360→4416. Right module, right GPU, same broken picture.

**Hunt.** Two false starts worth recording because both were *measurements*, not guesses, and both were
still wrong. First, the bloom target's alpha was flat 1.0, which pointed at the blur's radial attenuation
scalar arriving as zero; the guest's constant buffer holds the correct `0.75` at dword 2, so the suspicion
moved to delivery. An experiment knob forcing a re-upload of every constant buffer on every cache hit
changed nothing. Second, the two constant buffers for that draw sit 0x48 bytes apart and the *vertex*
buffer's dword 2 is exactly `0.0` — so binding the wrong one at set 0 / binding 6 would produce precisely
the observed output. That hypothesis was refuted not by argument but by fitting candidate blur kernels to
the dumped render-target PNGs: the intended vertical kernel fits at RMSE 1.62, the one implied by the
vertex buffer at 3.76. The pixel buffer does reach binding 6.

**Mechanism — two defects, both required.**

1. **The launch index resolved on the first read only.** In the shipped module, `v_and_b32 v1, 1, v0`
   loads `gl_VertexIndex`, but `v_and_b32 v0, -2, v0` loads a zero-initialised Function slot. Every ALU
   emitter untracks its destination *before* evaluating its sources, and a tracked register exists only in
   the tracker — so an in-place update reads zero. Y was pinned to −1 for all three vertices: a zero-area
   triangle, again. The regression test added with Entry 25 missed it because its shader writes a
   *different* VGPR on the second read than the shipped shader does. A test that exercises the mechanism
   but not the shape does not cover the shape.

2. **`VGT_PRIMITIVE_TYPE` was never modelled.** The uconfig register `0xC242` reads `0x11` =
   `DI_PT_RECTLIST` for exactly the five fill draws and `0x04` elsewhere; the repo contained zero
   references to it and hardcoded `TRIANGLE_LIST`. A rect list's three vertices are three **rectangle
   corners**. The fill VS emits `(-1,-1)`, `(1,-1)`, `(-1,1)` — the full screen as a rectangle, the
   lower-left half as a triangle. So even with (1) fixed, every guest clear would have covered half its
   target.

**Consequence.** `untrack_vertex_index` now spills the builtin into the register's slot before untracking,
at all eleven sites plus the MUBUF destination. `PipelineKey` carries a topology derived from
`VGT_PRIMITIVE_TYPE`; a rect list builds a triangle-strip pipeline and a non-indexed rect draw is issued
with four vertices. The fourth corner is taken from index 3 rather than computed as `p2 + p1 - p0`; the two
coincide for the index-derived idiom every fill here uses, and the approximation is documented at the site.
The snapshot records per-draw `topology` — the single field that distinguished a rect fill from an ordinary
three-vertex draw, and that appeared nowhere before.

**The generalisable lesson.** This is the FIFTH hardware register found to change results while being read
nowhere, after `CB_TARGET_MASK`, `CB_SHADER_MASK`, `CB_COLOR_CONTROL` and `SPI_PS_INPUT_CNTL`. Three were
harmless and two were the bug, which is exactly why the category stops being suspected. The other half of
the lesson is about masking: five independent defects sat between the guest's intent and the screen here,
and every one of the earlier fixes looked like a failure on its own because a later defect still swallowed
the result. "The picture did not change" is evidence about the *last* defect in the chain, never about the
fix you just landed — establish separately whether the fix reached the hardware, or you will revert
correct work.

---

## Entry 27 — a PS binds ONE texture per `image_sample` DESCRIPTOR, not per shader; and console ground truth is what proved the colour registers innocent (2026-07-21, task-199 / task-200)

**Trigger.** In-game Celeste rendered the night sky OLIVE and the moon YELLOW, while the gameplay tilemap
render target was pixel-correct with proper blues. A 320x180 target was being filled with a solid
`(0.498, 0.498, 0.0, 1.0)` read from a pixel constant buffer, and that RT was composited over the sky.
Three sessions of emulator-side reasoning had treated the olive constant as the suspect.

**Hunt.** A real-PS4 scrape of the same scene (`dumps/scrape2`, 80 frames of DCB + referenced-buffer
probes) settled it in one run. A byte scan found the SAME olive quad on the console — so the constant is a
legitimate guest intermediate, faithfully reproduced, and the divergence had to be downstream. Replaying
the console DCB's register writes and snapshotting the file at each draw showed the console frame is our
frame: **29 draws, matched 1:1 on kind, target extent and `CB_BLEND0_CONTROL`**, and *every* colour
register agreed byte-for-byte — `CB_BLEND0_CONTROL` at all 29 ordinals, `CB_TARGET_MASK` `0x0000ffff`,
`CB_SHADER_MASK` `0x0000000f`, `SPI_SHADER_COL_FORMAT` `0x00000004` (FP16_ABGR), `CB_COLOR0_INFO`
`0x8028`/`0x8828` switching at the same ordinals. That single result killed the entire
blend/format/swizzle hypothesis class, which is where the effort had been going.

**Mechanism.** The olive buffer is not a colour: it is a **neutral displacement map**, cleared to
`(0.5, 0.5, 0)`. Draw 14's pixel shader samples it, perturbs its UVs (`v_sin_f32` / `v_madmk_f32`), then
samples the SCENE at the displaced UV and exports **only that second result**:

```
s_load_dwordx8 s[16:23], s[12:13], 0x0            ; T# fetched from MEMORY = displacement map
image_sample v[0:2], v[3:4], s[16:23], s[24:27]   ; read displacement
...
image_sample v[0:3], v[0:1], s[0:7],   s[8:11]    ; SCENE, T# inline in user SGPRs
exp mrt0, ... done compr                          ; <- the export
```

The MIMG `srsrc`/`ssamp` operands are **per-instruction**, so one PS routinely mixes a *register-resident*
T# (loaded into user SGPRs by the launch ABI) with a *memory-resident* one (`s_load_dwordx8` through a
user-data descriptor-set pointer). Our recompiler memoised a single `ps_texture` and returned it from
`ensure_ps_texture` for every later sample, so only the FIRST sample's `DescriptorSource` ever reached
`io_samplers` — one `OpTypeImage` variable at set 0 / binding 1, loaded by both `OpImageSampleImplicitLod`s.
The executor bound the descriptor the first sample named (the displacement map), and the export therefore
read the displacement map: a full-screen olive quad, blended `ONE`/`ONE_MINUS_SRC_ALPHA` at alpha 1, which
*replaces* the sky. Draw 23 then additively added the moon glow onto `(0.5,0.5,0)` → a yellow moon.

The same collapse hit the **present** pass independently: draw 28 samples the frame through a
register-resident T# and then does a two-slice fetch of Celeste's **256x16 colour-grade LUT** through a
memory-resident one. The LUT was never bound, so every presented pixel was mis-graded even apart from the
sky.

**Consequence.** A PS now declares one combined image-sampler per DISTINCT `(T# provenance, S# SGPR)` pair,
in first-sample order — texture 0 keeps binding 1 (single-texture modules are emitted **byte-identically**,
verified by diffing the SPIR-V word stream against `HEAD`), extras take bindings 7+. The whole chain
carries the list: `IoLayout::samplers` → `ResourceSignature::textures` → `CreatePipeline::textures` → one
`COMBINED_IMAGE_SAMPLER` per entry in the set-0 layout → one `BindTexture` and one descriptor write each,
with the `needs_texture` draw guard counting rather than testing a bool. A repeat sample through the SAME
pair still shares one binding, so the nine-tap composite is unchanged. Snapshot `sampled` became an ARRAY.

**Tooling.** The console-vs-us differ built for this became permanent as
`cargo run -p ps4-gnm-scrape-host --bin framediff` (task-200, documented in
`tools/ps4-gnm-scrape/SETUP.md` §7). It reproduces the whole diagnosis in one command, and it is worth
knowing that it prints, per draw, `registers : identical on every register both sides recorded`.

**The generalisable lesson.** Two, and the first is the expensive one. *A faithfully-reproduced
intermediate is evidence about the producer, not the consumer.* The olive constant was correct on both
sides; every session that stared at it was reading a true fact that pointed the wrong way. Ground truth
that spans BOTH sides — the same scene on console and on us, compared per draw — converts that from a
question into a two-line answer, and it is worth the cost of building the capture path.

The second: **the recompiler's resource ABI is a place where "the corpus declares exactly one" quietly
becomes "exactly one is legal".** The doc comment on `IoLayout::samplers` asserted the ABI outright, the
executor had a matching strict-or-defer branch for `>1` that had *never fired*, and the differential
harness asserted `samplers.len() == 1`. Three layers agreed with each other and none of them agreed with
the hardware. When a guard has never fired, that is not evidence it is right — check whether the code
upstream can even reach it.

---

## Entry 28 — the RT-as-texture path bound a fixed linear/repeat sampler, and the capture recorded the guest's REQUEST rather than our BIND (2026-07-21, task-201)

**Trigger.** With the sky fixed (Entry 27) the whole frame read as BLURRED. Celeste renders at 320x180
and upscales to 1920x1080, so its pixel art must be point-sampled.

**Hunt.** No hunt was needed in the emulator; the code said it outright. `bind_render_target_as_texture`
carried a task-56 shortcut — *"A render target is sampled with the portable-default sampler
(linear/repeat); the S# filter/wrap refinement is out of scope for the RT path"* — while the two
plain-texture bind paths had honoured the guest's S# since task-173. Celeste's entire composite chain
and its final upscale are RT-as-texture, so all of them were force-bilinear.

The real work was refusing to guess the replacement. `framediff` was extended to decode the S# from the
console capture (GFX6/7 sampler layout: `word0[2:0]` = `CLAMP_X`, `word0[5:3]` = `CLAMP_Y`,
`word2[20]` = `XY_MAG_FILTER`), reading the register-resident sampler from PS user-data slots 8..11 and
the memory-resident one from the descriptor set at `ptr + 0x20` — the offset the shaders' own
`s_load_dwordx4 ..., 0x8` implies, SMRD immediates being dword indices. Across the 16 sampling draws of
one steady-state frame:

```
11x NEAREST/ClampToEdge      4x LINEAR/ClampToEdge      1x NEAREST/Repeat
draw 28 (320x180 -> 1920x1080 upscale):  NEAREST/ClampToEdge/ClampToEdge
```

**Mechanism.** Two separate wrongs in one hardcoded descriptor: the filter (LINEAR where the guest asked
NEAREST) and the wrap (Repeat where *no* RT-sampling draw in the frame asks for Repeat — they are all
ClampToEdge). The fix is a single `sampler_desc_for(Option<&SamplerState>)` used by every bind path, with
the portable default surviving only as the genuine no-S# fallback.

**Consequence.** The capture gained a `sampler_bound` field beside `s_sharp`. `s_sharp` is what the guest
REQUESTED; `sampler_bound` is what the backend was told to create. Both are populated from the same pure
helper the bind calls, so the record cannot drift from the GPU, and `framediff` now compares the console's
S# against our *bind* rather than our *request*.

**The generalisable lesson.** The capture had been recording the right number in the wrong column. Every
snapshot of every blurred frame faithfully showed `"bilinear": false` — the guest's request — while the
GPU filtered bilinearly, and `descriptor_honoured: false` was the only hint, buried in a field about the
*image*. A diagnostic that records what was ASKED FOR and not what was DONE will confirm your
expectations forever. Record the value you actually handed the hardware, next to the value you were
given, and let the two disagree in public.

The second lesson is about the shape of the fix. The obvious reading of "pixel art is blurred, it must be
NEAREST" would have forced nearest everywhere and silently broken the four bloom draws in the same frame
that legitimately ask for LINEAR. The hardware capture is what turned a plausible fix into the correct
one — the cost of consulting it was minutes, and it changed the answer.

---

## Entry 29 — Celeste submits a 4 MB command buffer every flip, and we copied it twice before reading it (2026-07-21, task-208/209)

**Trigger.** Celeste ran at 20 fps in gameplay and 25 fps in the attract scene, and two consecutive
optimisations had moved wall time between phases without shortening the frame. Rather than cut again
blind, the frame itself was instrumented (task-209): the flip-to-flip wall time of the flipping guest
thread, split into guest code / the flip syscall / other syscalls / run-loop bookkeeping, on that thread
only — process-wide sums across guest threads exceed 100% of wall and cannot answer "where does a frame
go".

**Hunt.** The split closed to 0.0% unaccounted immediately and named the culprit without ambiguity:
`flip 23.8 ms` of a `39.6 ms` attract frame, of which `18.1 ms` was PM4 *decode + free*. Celeste submits
`dcb_size = 4194300` bytes per flip — one dword short of 4 MiB, overwhelmingly padding — and the executor
turned it into a `Vec<u32>` copy of the whole buffer, then into ~525k `OwnedPacket`s each carrying its own
heap-allocated body. 13.1 ms to build them, 5.0 ms to free them, every frame.

**Mechanism.** None of it was necessary. The command buffer is identity-mapped (guest ptr == host ptr) and
PM4 is a dword stream, so a 4-byte-aligned buffer can be reinterpreted where it lies and walked as a
borrowing iterator — which the module already had, in `decode()`. The owning path existed because a doc
comment claimed the transient dword buffer could not be borrowed out; that was true of the copy the
function itself made, not of the guest buffer.

**Consequence.** `decode 13.098 → 0.000`, `packet_free 5.038 → 0.000`, the walk absorbing the decode at
`0.805 → 1.769`, the flip `23.83 → 8.36 ms`. Unlike the two changes before it the time did **not**
relocate: the attract frame went `39.6 → 23.3 ms`, 253 → 429 frames per 10 s window, 25.3 → 42.9 fps.

**The generalisable lesson.** The two failed attempts differed from this one in exactly one respect: they
were aimed at a phase nobody had measured against the *frame*. A phase budget that closes to 0.0% proves
the phase is understood; it says nothing about whether the phase is on the frame's critical path. Measure
the frame first, then the phase — and the check that a cut worked is frames per window, never the row that
was cut.

The instrumentation answered a second question on the way past. 212k guest VM exits per second looked like
a plausible explanation for the time outside the flip, so the run loop was made to measure one exit/entry
round trip directly (a guest stub of `mov eax,ID / syscall / dec rdi / jnz`, answered inside the loop
without entering the HLE dispatcher): **133 ns profiled, 65 ns unprofiled**. The flipping thread issues
~4000 syscalls per frame, so the entire VM-exit tax on the frame is ~0.26 ms — around 1%. The traffic is
real, but it is on threads that do not gate the frame. The 14.3 ms outside the flip is genuine guest CPU
work, and a rate on its own was never evidence of a cost.

## Entry 30 — the EOP memory-fence label is a TWO-WAY discriminator: one title collapses if you write it, another deadlocks if you don't — and the tell is which channel the guest LISTENS on (2026-07-23, task-157 follow-up, Little Nightmares bring-up)

**Trigger.** Entry 20 / doc-5 case 24 (task-157) established that withholding the inline EOP
memory-fence label keeps Celeste's gnmx re-recording per-frame texture binds — the fix for the
white-logo collapse. The default became "never write the label; completion reaches the guest
through the equeue it blocks on." A second title (Little Nightmares, an Unreal Engine 4 title,
CUSA05952) then deadlocked at boot: 30 threads parked, no frames, forever.

**Hunt.** The stall-diagnosis toolbox (doc-4 §3.10, added the same day) named it without a
debugger. The profiler's in-flight list — now carrying guest thread *names* — showed
`SubmitDoneAsyncTaskThreadPS4` blocked in `scePthreadMutexLock`; the `[SYNC]` stuck-lock
reporter named the holder: *tid 1 (Thread-1) has held mutex 0x4d72d78 for 54 s*. exectrace's
"last syscall before the silence" on tid 1 read `sceGnmSubmitCommandBuffers`, and its RIP
histogram was 99% one address — a spin, inside guest code, immediately after a submit. The
guest submitted, then spun waiting for that submit to complete, holding a lock its own
submit-done thread needed. A one-line experiment closed it: `UNEMUPS4_GPU_EOP_SYNC=1` (write
the label inline, the pre-task-157 behaviour) un-wedged the title instantly. It polls the EOP
memory label as its **only** completion signal.

**Mechanism — the two titles are mirror images.**

| | reads completion from | needs the EOP label |
|---|---|---|
| Celeste (gnmx) | the equeue it blocks on (`sceKernelWaitEqueue`, 1577× / 40 s) | **withheld** — writing it makes gnmx recycle buffers without re-emitting binds (case 24) |
| Little Nightmares (UE4) | the EOP memory label it spins on (0 equeue waits) | **written** — withholding it hangs the submit thread |

Same packet, opposite requirements. The discriminator is *which channel the guest listens on
for GPU completion*. And it is not readable from registration: **both** titles call
`sceGnmAddEqEvent` at boot. The UE4 title registers an equeue event and then never once waits
on it — it polls the label instead. Only the **wait** (`sceKernelWaitEqueue` actually
collecting a completion) proves the queue is the channel; registration proves nothing.

**The trap that cost a visible regression.** The first fix gated the label on whether the guest
had *ever waited* on an equeue, set on the first `sceKernelWaitEqueue`. That flag is decided
**too late**. An equeue title has not reached its first wait during its first ~3 submits, so a
wait-gated write hands it the label for those boot frames — and three collapsed frames at boot
are enough for gnmx to enter its recycle-skip path and stay collapsed for the whole run.
Celeste came back on screen as white boxes with debug text. The lesson: when a per-frame
decision defaults to the *wrong* thing until a late signal flips it, the boot frames are
already spent on the wrong branch. **A safety-critical default must be right from frame 0, not
from the frame that finally proves the title's identity.**

**The fix that holds.** Default WITHHOLD, unconditionally — the Celeste-safe branch, correct
from the first frame. Write the label only once a title has **positively shown** it is a
poller: no equeue completion ever collected, AND more than a 1 s boot grace elapsed since its
first EOP submit. An equeue title trips the "collected a completion" flag within milliseconds,
far inside the grace, so it takes the withhold path on every frame including the first. A
poller never collects, so after the grace it takes the write path and un-wedges — at the cost
of a ~1 s boot stall that only it ever pays. The grace threshold is structural, not a tuned
timeout: any value above an equeue title's first-wait latency (~50 ms) and below human patience
works; it only ever delays a title that was going to poll anyway.

**Consequence.** Celeste verified textured on screen (the maintainer's eyes — a log cannot see
white boxes). Little Nightmares reached 7 presented frames (was 2) and a live flip thread
before its next wall. The discriminator lives in `ps4_core::gpu::should_write_completion_label`;
the equeue-collection flag is set from `sceKernelWaitEqueue`, not `sceGnmAddEqEvent`.

**The generalisable lesson.** When two guests want opposite things from one mechanism, do not
look for a switch you can read at the moment you must decide — at the first submit the two
titles are genuinely indistinguishable (both registered, neither waited). Instead pick the
default that cannot harm the working title, and switch away from it only on **positive proof**
that accrues over time. "Prove you are the exception" beats "guess which one you are", because
the guess is wrong exactly when it is most expensive: at boot, on the title that already works.

## Entry 31 — deriving a PM4 emitter from the console's own command stream: the scrape → decode → match → pin method (2026-07-23)

**Trigger.** The shader-set / draw emitters (`pm4/emit.rs`) needed their command layout —
the SH-run grouping, the trailing-NOP size, the draw-packet shapes — established from a source
we independently hold and can re-check, not asserted from memory.

**The ground truth.** The GoldHEN scraper (task-168) streams the real DCB Celeste submits, per
flip, off the console → `~/celeste-scrape-oracle/frameNNN_sub0_flip_dcb.bin`. That byte stream
is what Sony's gnmx actually emits; we have our own copy, so the layout can be read straight
off the hardware's output.

**The method (repeatable for any emitter):**
1. **Decode** a real DCB with `dcbdump` (`tools/ps4-gnm-scrape/host`, `cargo run -p
   ps4-gnm-scrape-host --bin dcbdump -- <file>`). It runs the capture through the emulator's
   own PM4 decoder (`ps4_gnm::pm4::decode` + `pm4::opcodes::reg_name`) and prints every packet
   readably: opcode names, `SET_*_REG` runs resolved to register names, draws, collapsed NOP
   runs. The decoder is built only from the AMD PM4 Type-3 header format, so it is a clean lens
   on the bytes.
2. **Read** the structure off the dump. For the shader set, `dcbdump` shows two `SET_SH_REG`
   runs — `{PGM_LO=0x028e8001, PGM_HI=0x00000000}` at SH reg `0x48`, `{RSRC1=0x002c0000,
   RSRC2=0x0}` at `0x4a` (header `0xC0027600`, each a run-of-2, PGM_HI written 0). After each
   set: `NOP x12` = an 11-dword `IT_NOP` data block (header `0xC00A1000`). Draws:
   `IT_DRAW_INDEX_AUTO [00000003 00000002]` (count, initiator) and
   `DRAW_INDEX_OFFSET_2 [max, off, count, init]` (header `0xC0033500`).
3. **Match** the emitter to that structure. `set_vs_shader`/`set_ps_shader` group PGM_LO/HI
   then RSRC1/2 as two SET_SH runs and force PGM_HI 0; the capture confirms each choice against
   the console rather than leaving it a bare assertion.
4. **Pin** it with a differential witness test. `emit_matches_console_pm4_capture` asserts our
   emit output equals the captured dwords (headers `0xC0027600` / `0xC00A1000` / `0xC0012D00`
   / `0xC0033500`). If the emitter ever drifts from what the console does, the test fails.

**A divergence is a finding, not a failure.** The console writes `DRAW_INDEX_AUTO
draw_initiator = 2` (VGT source-select = auto-index); we write 0, because the software executor
auto-generates the indices and ignores source-select. The test documents the divergence
explicitly instead of hiding it — a marked place to revisit if a title ever depends on the
initiator.

**Why the capture is the right oracle.** Reproducing the console's own bytes shows the layout
is a hardware/library fact taken from ground truth we hold; the differential test makes that
claim executable and re-checkable; and `dcbdump` keeps the oracle inspectable by eye. The same
method extends to any packet an emitter builds — capture, decode, match, pin.
