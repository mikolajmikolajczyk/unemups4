---
id: doc-2
title: GPU approach research
type: other
created_date: '2026-07-10 17:59'
---

# GPU approach research

Research only — no implementation decided. This doc surveys what PS4 software actually needs from the GPU stack, how existing emulators built theirs, and what realistic options exist for unemups4 given its ethos (lightweight, educational, solo-maintained, Rust, explicitly *not* a faithful reimplementation). It ends with a recommendation and candidate backlog tasks. The maintainer decides; nothing here is committed work.

Claims are cited inline. Statements marked *(inference)* or *(speculation)* are exactly that.

## 1. Current state (verified in-repo, 2026-07)

- **Presentation-only GPU.** `crates/gpu` (ash 0.38, raw Vulkan) receives `GpuCommand`s over a crossbeam channel and presents guest framebuffers as a textured full-screen quad. No draw execution of any kind.
- **libSceVideoOut HLE** (`crates/libs/src/libscevideoout/mod.rs`): `sceVideoOutOpen`, `RegisterBuffers`, `SubmitFlip`, `SetFlipRate`, `AddFlipEvent`, `GetFlipStatus`, `SetBufferAttribute`. Vsync-capped 60 fps after recent perf work (task-16/17/19); a zero-copy `VK_EXT_external_memory_host` import path is designed but unimplemented (task-18 — blocked on the headless devShell having no Vulkan driver).
- **Zero Gnm.** No `libSceGnmDriver` handlers, no PM4, no Liverpool, no GCN — not even stubs. `grep -ri 'gnm\|pm4\|liverpool' crates/` finds nothing. Glossary and status.md record this explicitly ("a guest issuing real GNM draw calls shows nothing").
- **Proof of the working tier:** `examples/ps4-softgpu` renders on the CPU into a registered buffer and flips — uses only `sceVideoOutOpen/RegisterBuffers/SubmitFlip` + `sceKernelUsleep`. This is the entire graphics surface unemups4 supports today.
- **Identity mapping is an asset:** guest pointers are host pointers, so a future PM4 parser or shader decoder can read command buffers and shader binaries out of guest memory directly, with no address translation layer.

## 2. What PS4 software needs from the GPU, by tier

### Tier A — framebuffer-only homebrew (works today)

CPU renders pixels into a buffer; `libSceVideoOut` presents it. This is not a fringe pattern — it is the **primary graphics path in the OpenOrbis SDK's own samples**. The SDK's flagship `samples/graphics` (Mandelbrot) is pure CPU-to-framebuffer with no shader files at all ([OpenOrbis samples/graphics](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/tree/master/samples/graphics), [CHANGELOG v0.0.46](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/blob/master/CHANGELOG.md)). A large slice of real homebrew needs zero GPU emulation — just correct videoOut semantics (flip timing, flip events wired to equeues, buffer attributes).

### Tier B — GPU-accelerated homebrew (Gnm / Piglet)

Two structural facts dominate this tier:

1. **OpenOrbis ships no working native Gnm 3D sample.** A "GNM Command Driver" is an *open issue* on their tracker — the driver needed to use Gnm for 3D from homebrew was never finished in-tree ([OpenOrbis issue #5](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/issues/5)). The SDK's actual GPU-accelerated sample is `samples/piglet` — OpenGL ES 2.0 via Sony's Piglet layer ([CHANGELOG v0.5.2](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/blob/master/CHANGELOG.md), [samples/piglet](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/tree/master/samples/piglet)).
2. **GPU homebrew ships precompiled GCN shader binaries.** Retail firmware strips the runtime shader compiler (`libScePigletv2VSH` compiler removed, `libSceShaccVSH` absent entirely); the piglet sample links a library literally named `PrecompiledShaders`. Runtime GLSL compilation requires side-loading devkit modules ([flatz write-up via psx-place](https://www.psx-place.com/threads/opengl-es-implementation-on-the-ps4-write-up-provided-by-flatz.21600/), [psxhax](https://www.psxhax.com/threads/opengl-es-for-ps4-writeup-and-playstation-4-gl-test-by-flat_z.6156/)). Community tooling exists specifically to dump these precompiled GCN blobs ([PSSL shader dumper](https://www.psxhax.com/threads/ps4-opengl-pssl-pre-compiled-shader-dumper-by-theorywrong.6590/)).

**Consequence (the crux of this whole doc):** shaders reach the emulator as **GCN machine code**, not as source. Any GPU tier beyond framebuffer HLE must *handle GCN ISA somehow* — translate it, interpret it, or pattern-match known blobs. There is no "intercept a high-level shader string" shortcut.

### Tier C — commercial games

Full GCN 2.0 "Liverpool": tessellation, geometry shaders, 8 ACEs of async compute, three command-buffer types (DCB draw / CCB constant-engine / ACB async-compute), texture tiling, broad GCN opcode coverage ([PS4 tech specs](https://en.wikipedia.org/wiki/PlayStation_4_technical_specifications), [shadPS4 command processing](https://deepwiki.com/shadps4-emu/shadPS4/4.1-graphics-command-processing)). This tier is the province of shadPS4-class projects and is out of scope for unemups4's ethos; it is included only to calibrate effort estimates.

## 3. How existing emulators do GPU

### The shared architectural fact: Gnm is statically linked

`Gnm`/`Gnmx` are static libraries compiled into the game executable — an emulator **never sees Gnm API calls**. GPCS4's design doc states it plainly: "Gnm libraries are all static libraries compiled into the game's main executable. So we won't receive any Gnm draw/state call directly from the game" ([GPCS4 GraphicsStack.md](https://github.com/Inori/GPCS4/blob/master/Doc/GraphicsStack.md)). What crosses the dynamic-linking boundary — the only interceptable surface — is **libSceGnmDriver**, whose submission entry points hand over guest-memory command buffers full of PM4 packets. Key NIDs (verified against shadPS4's `gnmdriver.cpp`):

| Function | NID | Role |
|---|---|---|
| `sceGnmSubmitCommandBuffers` | `zwY0YV91TTI` | submit DCB/CCB pairs |
| `sceGnmSubmitAndFlipCommandBuffers` | `xbxNatawohc` | submit + queue scanout flip |
| `sceGnmSubmitDone` | `yvZ73uQUqrk` | end-of-batch sync point |
| `sceGnmDrawIndex` / `DrawIndexAuto` | `HlTPoZ-oY7Y` / `GGsn7jMTxw4` | packet *builders* — write PM4 into a cmdbuf |
| `sceGnmDispatchDirect` | `0BzLGljcwBo` | compute dispatch packet builder |
| `sceGnmMapComputeQueue` / `DingDong` | `29oKvKXzEZo` / `bX5IbRvECXk` | async compute rings |

([shadPS4 gnmdriver.cpp](https://github.com/shadps4-emu/shadPS4/blob/main/src/core/libraries/gnmdriver/gnmdriver.cpp))

So "Gnm API-level HLE" does not exist as an option in practice. Every working emulator does the same thing: **hook the GnmDriver submit functions, then parse PM4 packets back into draw/state operations.** The real divide is HLE-parse-PM4 vs. true LLE (running actual CP microcode / an amdgpu-style device), and only one project attempts the latter.

### Per-emulator survey

**shadPS4** (C++20, the compatibility leader — [repo](https://github.com/shadps4-emu/shadPS4)). HLE at the GnmDriver boundary; the `AmdGpu::Liverpool` class parses PM4 Type-3 packets asynchronously on a GPU thread using C++20 coroutines, interleaving DCB/CCB/ACB streams; draws invoke a Vulkan rasterizer with a multi-level pipeline cache ([DeepWiki: graphics system](https://deepwiki.com/shadps4-emu/shadPS4/4-graphics-system), [command processing](https://deepwiki.com/shadps4-emu/shadPS4/4.1-graphics-command-processing)). Shaders: a full **GCN → IR → SPIR-V recompiler** modeled on yuzu's "Hades" compiler ([readonlymemo interview coverage](https://readonlymemo.com/ps4-emulator-rpcsx-dolphin-hdr-ntsc-pal-colors/)). Measured size: **~34.6k lines across 105 files** in `src/shader_recompiler/` alone ([tree](https://github.com/shadps4-emu/shadPS4/tree/main/src/shader_recompiler)) — frontend GCN decode + control-flow structuring, translate units per instruction class (scalar/vector ALU, scalar/vector memory, interpolation, data-share, export), an optimizing IR, and ~22 SPIR-V emission files. Documented pain points: render-target extents aren't cleanly in PM4 (recovered from trailing-NOP hint packets — an explicit hack), Constant Engine ↔ Draw Engine counter synchronization, texture tiling. The recompiler was the milestone that flipped "many games" to booting (v0.1.0).

**GPCS4** (C++, historically important, superseded — [GraphicsStack.md](https://github.com/Inori/GPCS4/blob/master/Doc/GraphicsStack.md)). The clearest primary-source articulation of the pattern: intercept GnmDriver submits, "parse command buffers (PM4 packet queue)… recover the original Gnm calls," translate state to Vulkan, translate GCN bytecode → SPIR-V.

**fpPS4** (Free Pascal — [repo](https://github.com/red-prig/fpPS4)). Same family: GnmDriver interception + PM4 + GCN→SPIR-V over Vulkan *(classification partly inferred; the README doesn't spell out internals)*. Ran small commercial titles (Sonic Mania, Spelunky); currently mid-rewrite of its core. Second-best compatibility historically ([Emulation General Wiki](https://emulation.gametechwiki.com/index.php/PlayStation_4_emulators)).

**Kyty** (C++, early proof-of-concept — [repo](https://github.com/InoriRus/Kyty)). HLE + GCN recompilation over Vulkan 1.2, deliberately brute-force: "recompile all the shaders, untiling all the textures and upload all the buffers every frame" ([Emulation General Wiki](https://emulation.gametechwiki.com/index.php/PlayStation_4_emulators)). Ran some simple games; stalled early. Lesson: even the naive version of this stack demands the full GCN pipeline — brute force saves engineering on caching, not on shader translation.

**Obliteration** (Rust core — [repo](https://github.com/obhq/obliteration)). Forked from Kyty, then repositioned as an experimental PS4 *kernel*; "cannot run any games yet," no working GPU pipeline to learn from. Relevant only as evidence that a Rust PS4 project chose to defer GPU entirely.

**RPCSX** (C++, PS4 *and* PS5 — [repo](https://github.com/RPCSX/rpcsx)). The LLE outlier: full-system design (`kernel/`, `gpu/`, `cpu/`, VFS, IPMI) with a separate GPU-emulation component closer to emulating the amdgpu device *(inferred from repo layout and RPCS3 lineage; the README doesn't state the boundary — treat as reasoned inference)*. More ambitious, notably less far along on PS4 compatibility than shadPS4.

**Takeaway:** one proven blueprint (HLE-parse-PM4 + GCN→SPIR-V + Vulkan), one over-ambitious outlier, and one Rust project that punted on GPU. The blueprint's minimum viable form is still enormous: PM4 register-state machine + tiling + a shader translator.

## 4. Rust ecosystem inventory

- **rspirv** ([repo](https://github.com/gfx-rs/rspirv)) — pure-Rust SPIR-V builder/parser/disassembler. The most direct fit for emitting SPIR-V from a hand-written GCN decoder; you control control-flow structuring yourself.
- **naga** ([in wgpu tree](https://github.com/gfx-rs/wgpu/tree/trunk/naga)) — multi-backend shader translation (emits SPIR-V/WGSL/MSL/HLSL). Its IR is public but the validator demands structured control flow; translating unstructured GCN branching into naga IR is a known friction point, and naga has rejected valid-but-unusual SPIR-V ([wgpu #5598](https://github.com/gfx-rs/wgpu/issues/5598)). *(inference: rspirv is lower-friction for machine-generated shaders.)*
- **wgpu vs ash** — wgpu buys portability and removes barrier boilerplate, but interposes naga validation on the SPIR-V path and constrains low-level control ([not a Vulkan replacement](https://medium.com/@spencerkohan/webgpu-is-not-a-replacement-for-vulkan-yet-233ba1bb7829)). unemups4 already has a working ash backend and task-18 depends on `VK_EXT_external_memory_host`, which wgpu does not expose; staying on ash is the path of least resistance. *(judgment call, not a settled fact.)*
- **GCN decoders in Rust: none.** No crates.io crate decodes GCN shader ISA. Nearest references are all non-Rust: shadPS4's recompiler (C++), CLRX assembler/disassembler covering GCN 1.0–1.4 ([CLRX](https://github.com/CLRX/CLRX-mirror)), AMD's machine-readable ISA XML + IsaDecoder ([GPUOpen](https://gpuopen.com/learn/using-isadecoder-api/)), and the canonical [Southern Islands ISA manual](https://docs.amd.com/v/u/en-US/southern-islands-instruction-set-architecture). Any GCN decoding here is written from scratch, porting semantics from those references.
- **PS4/PSSL tooling in Rust: none found.** Green field.

## 5. Options

### Option A — stay videoout-only, polish presentation

Finish the current tier: task-18 zero-copy import, flip events actually wired to equeues (today `AddFlipEvent` returns success and `WaitEqueue` sleeps), real `GetFlipStatus` counters, buffer-attribute honoring (pixel format, pitch), resolution flexibility.

- **Unlocks:** nothing new — but hardens the tier that covers the OpenOrbis SDK's own primary graphics path and all of this repo's examples.
- **Effort:** days-to-weeks, incremental.
- **Risks:** none architectural. Headless devShell (no Vulkan driver) makes runtime verification maintainer-only — already a known task-18 constraint.
- **Crates:** existing ash stack.

### Option B — "Gnm API-level HLE, no GCN ISA" — largely infeasible as imagined

The research kills the attractive version of this option twice over:

1. Gnm is statically linked — there are no API calls to intercept, only GnmDriver submits carrying PM4 (§3).
2. Shaders arrive as precompiled GCN binaries — there is no high-level shader representation to translate at any API boundary (§2, Tier B).

What survives of Option B is Option D below: PM4 parsing with something-less-than-a-full-recompiler for shaders. Recording this dead end explicitly is one of this doc's main contributions — it should save a future session from re-deriving it.

### Option C — full PM4 + GCN→SPIR-V recompiler (the shadPS4 path)

- **Unlocks:** Tier B fully, Tier C eventually.
- **Effort:** the shader recompiler alone is ~35k LOC in shadPS4, built by a team reusing a proven compiler design; add the Liverpool register-state machine, tiling, CE/DE sync, pipeline caching. For a solo maintainer this is a multi-year project that *becomes* the project.
- **Risks:** directly contradicts the project's stated ethos ("lightweight, educational, research — not a faithful reimplementation"). It would also duplicate shadPS4 poorly rather than teach anything shadPS4's source doesn't already teach.
- **Verdict:** documented so the cost is on record; not recommended.

### Option D — incremental PM4 subset (recommended direction)

Hook `libSceGnmDriver` submits and grow a PM4 understanding in deliberately small, educational stages, reusing the existing softgpu/present infrastructure. Each stage is independently demonstrable:

- **D1 — boot-and-trace.** Stub the GnmDriver NIDs (§3 table) to log-and-succeed; add a PM4 Type-3 packet decoder that *traces* the command stream (opcode names, register writes) without executing anything. A Gnm-using guest boots instead of crashing on unresolved imports, and its GPU intent becomes visible in logs. Pure decoding — no Vulkan work, testable headless, very much in this project's educational spirit (compare: the syscall table with names from `ps4_names.txt`).
- **D2 — present/sync subset.** Execute the non-shader packets: `SubmitAndFlipCommandBuffers` routed into the existing flip path, end-of-pipe (EOP) event packets completing to equeues, `SubmitDone`, CPDMA copies (guest-memory blits — trivial under identity mapping). Unlocks homebrew that uses Gnm only for synchronization/present around CPU rendering.
- **D3 — shaders, the cheap ways first.** Two sub-options, not mutually exclusive:
  - *Blob matching:* recognize specific precompiled shader binaries by hash and map them to handwritten host equivalents (SPIR-V or even fixed CPU raster paths). Viable because the homebrew shader universe is tiny (piglet's `PrecompiledShaders` set is finite). Ugly but honest about being HLE; classic early-emulator technique. *(speculation: adequacy depends on how few distinct blobs real homebrew uses — needs a survey once D1 tracing exists.)*
  - *GCN interpreter on the CPU:* decode the small GCN subset simple VS/PS shaders use (scalar/vector ALU, exports, a few memory ops per the SI ISA manual) and interpret it per-vertex/per-pixel into a software rasterizer — an extension of the existing softgpu philosophy, presented through the existing Vulkan quad. Slow, but *educationally the most valuable artifact this project could produce*, and a Rust GCN decoder would be a genuinely novel open-source contribution (§4).
- **D4 (optional, far)** — feed the D3 decoder into rspirv to emit real SPIR-V for the subset, executing on the ash backend. Only worth it if D3 works and performance actually matters for some target homebrew.

- **Unlocks:** D1–D2: Gnm-touching homebrew boots and presents. D3: first real GPU-drawn triangles from guest-supplied shaders. Never Tier C — that is explicitly out of scope.
- **Effort:** D1 weeks; D2 weeks; D3 months (the decoder is the long pole even for a subset); D4 open-ended.
- **Key risks:** (1) **test corpus** — OpenOrbis has no native Gnm sample, so D1/D2 need hand-crafted test ELFs that write raw PM4 packets (feasible: PM4 packet formats are documented in shadPS4/GPCS4 source and AMD docs; fits the repo's existing examples/ pattern). (2) Scope creep toward Option C — mitigated by phase gates and by recording Tier C as deferred. (3) Headless devShell can't run Vulkan — D1 is immune (pure decode), D2+ needs maintainer-side verification, same as task-18.
- **Crates:** existing ash backend; rspirv only at D4; no external GCN crate exists to lean on (§4).

## 6. Recommendation

> **Maintainer revision, 2026-07-10 (supersedes the original recommendation below; see [decision-3](../decisions/decision-3%20-%20GPU-direction-Bloodborne-north-star-phased-PM4-path-portable-Vulkan.md)).**
>
> **North star: run Bloodborne (Tier C).** The phased path stands and each phase
> must visibly work on its own, but the phases now continue *through* the full
> GCN→SPIR-V recompiler rather than stopping short of it. The recompiler is a
> **late, mandatory phase — not deferred.** This is a multi-year north star, not
> the next milestone.
>
> - Phasing unchanged: Option A (Tier A / videoout) now → Option D incrementally
>   (D1 trace, D2 present/sync, D3 CPU shader interpreter) → **Option C, the full
>   recompiler, as the late phase toward Tier C**.
> - **CPU shader interpreter first, recompiler second, interpreter kept as the
>   correctness oracle** — mirroring the project's proven interp→JIT pattern from
>   the x86jit CPU migration (interpreter = correctness reference, recompiler =
>   speed, differential tests between the two).
> - **Backend: ash/Vulkan stays primary and must remain swappable.** New
>   constraint: keep the Vulkan layer portable to **MoltenVK** (macOS, arriving
>   in a few months) and potentially **native Metal**. Policy: prefer the Vulkan
>   portability subset; gate any non-portable extension behind a capability check
>   with a graceful fallback. Precedent: task-18's `VK_EXT_external_memory_host`
>   is unsupported by MoltenVK — its staging-copy fallback is exactly the
>   required gate-and-fallback pattern.
> - Option C is therefore **not** recorded in `deferred.md`; the Bloodborne
>   mission is the "change of project mission" the original deferral was
>   conditioned on.
>
> **Non-GPU workstreams toward Bloodborne (future, unscheduled — not tasks
> yet):** FSELF loading of decrypted game dumps; AJM audio; substantially broader
> libkernel coverage; savedata. Named here so the scope of the north star is
> honest; none are scheduled and none should be implemented unprompted.

---

*Original recommendation (retained for the record; superseded by the maintainer
revision above):*

**Option A now, then Option D incrementally, with Option C explicitly recorded as deferred.**

Rationale: Tier A is where the project's users (its own examples, the OpenOrbis mainstream) actually are, and it is not finished (task-18, flip events). Option D's early phases (trace, then present/sync subset) each produce something visibly working, cost weeks not years, are mostly testable headless, and build the in-repo understanding needed to make an informed later call on shaders. The D3 GCN-interpreter route fits the project's educational ethos better than any recompiler ever will — and if it stalls, D1/D2 remain useful standalone (Gnm homebrew boots and traces instead of crashing). Option C should go into `deferred.md` with "revisit when: never, absent a change of project mission."

### Candidate backlog tasks (NOT created — maintainer's call)

Phase 1 — finish Tier A:
1. `softgpu perf: zero-copy guest framebuffer` — already filed as task-18; land it.
2. `videoout: wire flip events to equeues` — `AddFlipEvent` registers a real event; `WaitEqueue` wakes on flip instead of sleeping; `GetFlipStatus` reports real counters.

Phase 2 — Gnm boots and traces (D1):
3. `gnm: stub libSceGnmDriver entry points` — log-and-succeed handlers for the submit/draw/dispatch NIDs so Gnm-linked homebrew boots.
4. `gnm: PM4 Type-3 packet trace decoder` — parse DCB streams at submit time; log opcode names and register writes; no execution. Headless-testable.
5. `examples: hand-written PM4 test ELF` — an example that builds a raw PM4 command buffer (clear + flip) and submits it; the repo's own Gnm test corpus.

Phase 3 — present/sync subset (D2):
6. `gnm: execute present/sync PM4 subset` — SubmitAndFlip → existing flip path; EOP events → equeues; CPDMA copies under identity mapping.

Phase 4 — shaders (D3, decide after Phase 3 retro):
7. `gcn: minimal GCN decoder + disassembly trace` — SI-subset decoder with human-readable disasm of guest shader blobs found via D1 tracing.
8. `gcn: CPU shader interpreter for one triangle` — interpret a trivial VS/PS pair through a software rasterizer into the existing present path.
9. *(stretch)* `gcn: rspirv SPIR-V emission for the interpreter subset` — only if D3 perf demands it.

Also: 10. `docs: record Option C (full recompiler) in deferred.md` — capture the §5C cost analysis as a deferral entry.

## 7. Open questions for the maintainer

**Answered by the maintainer 2026-07-10 (see [decision-3](../decisions/decision-3%20-%20GPU-direction-Bloodborne-north-star-phased-PM4-path-portable-Vulkan.md)). Questions kept verbatim; answers inline.**

1. **Ambition ceiling:** is Tier B (GPU homebrew drawing real triangles) actually a goal, or is a perfect Tier A + D1 tracing ("see what the guest wanted") enough for the project's research purpose? Phases 3–4 hinge on this.

   → **Tier C. The ultimate goal is running Bloodborne.** Phasing is unchanged (each phase must visibly work), but the ceiling is a full commercial title — this is a north star over years, not the next milestone. Tier B is a waypoint, not the destination.

2. **Test corpus:** are hand-written PM4 example ELFs (task 5 above) acceptable as the primary Gnm test vehicle, given OpenOrbis ships no native Gnm sample? Alternative — depending on piglet homebrew — drags in the whole GLES/Piglet layer.

   → **Yes, as the phase-2 corpus.** Hand-written PM4 test ELFs are the starting corpus. Later add captured real Gnm command buffers (from homebrew, eventually from the game) plus RenderDoc comparisons.

3. **Performance floor for D3:** is a CPU shader interpreter at single-digit fps acceptable as an educational milestone, or must first-triangle already run on the host GPU (which pulls D4 forward and roughly doubles the shader-phase cost)?

   → **Yes — CPU interpreter first, deliberately.** This mirrors the project's proven interp→JIT pattern from the x86jit CPU migration: the interpreter is the correctness oracle, the recompiler is speed, and the two are cross-checked with differential tests. Single-digit fps is fine for the interpreter milestone; the recompiler comes later.

4. **Backend commitment:** stay on ash (recommended here) or is wgpu portability desired badly enough to accept losing `VK_EXT_external_memory_host` and adding naga validation risk?

   → **Stay on ash/Vulkan (primary).** New constraint: the Vulkan layer must remain swappable to **MoltenVK** (macOS, arriving in a few months) and potentially **native Metal**. Policy: prefer the Vulkan portability subset; gate any non-portable extension behind a capability check with a graceful fallback. `VK_EXT_external_memory_host` (task-18) is the concrete precedent — MoltenVK doesn't support it, and its staging-copy fallback is exactly the pattern to follow.

5. **deferred.md:** confirm recording Option C as a permanent deferral, so future sessions don't re-litigate it.

   → **No — do NOT defer Option C.** The full GCN→SPIR-V recompiler is **un-deferred**: it is a late but mandatory phase toward Bloodborne. The Bloodborne mission is precisely the "change of project mission" the original permanent-deferral proposal was conditioned on. Option C must not be listed in `deferred.md`.

## 8. Sources

Emulators: [shadPS4](https://github.com/shadps4-emu/shadPS4) · [gnmdriver.cpp (NIDs)](https://github.com/shadps4-emu/shadPS4/blob/main/src/core/libraries/gnmdriver/gnmdriver.cpp) · [shader_recompiler tree](https://github.com/shadps4-emu/shadPS4/tree/main/src/shader_recompiler) · [DeepWiki: shadPS4 graphics](https://deepwiki.com/shadps4-emu/shadPS4/4-graphics-system) (AI-generated over the repo; structure cross-checked against source) · [GPCS4 GraphicsStack.md](https://github.com/Inori/GPCS4/blob/master/Doc/GraphicsStack.md) · [fpPS4](https://github.com/red-prig/fpPS4) · [Kyty](https://github.com/InoriRus/Kyty) · [Obliteration](https://github.com/obhq/obliteration) · [RPCSX](https://github.com/RPCSX/rpcsx) · [Emulation General Wiki: PS4 emulators](https://emulation.gametechwiki.com/index.php/PlayStation_4_emulators) · [readonlymemo on shadPS4/RPCSX](https://readonlymemo.com/ps4-emulator-rpcsx-dolphin-hdr-ntsc-pal-colors/)

Homebrew/SDK: [OpenOrbis toolchain](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain) · [samples/graphics](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/tree/master/samples/graphics) · [samples/piglet](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/tree/master/samples/piglet) · [GNM Command Driver issue #5](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/issues/5) · [flatz OpenGL ES write-up](https://www.psx-place.com/threads/opengl-es-implementation-on-the-ps4-write-up-provided-by-flatz.21600/) · [psxhax GL test](https://www.psxhax.com/threads/opengl-es-for-ps4-writeup-and-playstation-4-gl-test-by-flat_z.6156/) · [precompiled shader dumper](https://www.psxhax.com/threads/ps4-opengl-pssl-pre-compiled-shader-dumper-by-theorywrong.6590/)

Hardware/ISA: [PS4 technical specifications](https://en.wikipedia.org/wiki/PlayStation_4_technical_specifications) · [GCN (Wikipedia)](https://en.wikipedia.org/wiki/Graphics_Core_Next) · [AMD Southern Islands ISA manual](https://docs.amd.com/v/u/en-US/southern-islands-instruction-set-architecture) · [CLRX](https://github.com/CLRX/CLRX-mirror) · [GPUOpen IsaDecoder](https://gpuopen.com/learn/using-isadecoder-api/)

Rust ecosystem: [rspirv](https://github.com/gfx-rs/rspirv) · [naga](https://github.com/gfx-rs/wgpu/tree/trunk/naga) · [wgpu SPIR-V validation issue #5598](https://github.com/gfx-rs/wgpu/issues/5598) · [WebGPU vs Vulkan trade-offs](https://medium.com/@spencerkohan/webgpu-is-not-a-replacement-for-vulkan-yet-233ba1bb7829)
