---
id: doc-3
title: OpenOrbis graphics + shader capabilities
type: other
created_date: '2026-07-10 18:39'
---

# OpenOrbis graphics + shader capabilities

Research doc scoping the graphics/GNM/shader surface the OpenOrbis PS4 Toolchain
actually gives us, to ground **task-22** (hand-written PM4 test ELF) and the
phase-3/4 GPU work in `decision-3` / `doc-2 - GPU-approach-research.md`. Idea/research
only — no code landed. Companion to doc-2: doc-2 surveyed emulator blueprints and the
PS4 GPU stack in the abstract; **this doc grounds those claims in what is physically on
disk** (a full OpenOrbis checkout at `~/src/ps4labs/ps4sdk`) and pins down the shader
question that doc-2 left at the "shaders arrive as GCN machine code" level.

**Provenance tags** used throughout:
- `[LOCAL]` — confirmed by inspecting files under `~/src/ps4labs/ps4sdk` (the OpenOrbis
  checkout; `git remote` = `OpenOrbis/OpenOrbis-PS4-Toolchain.git`) or symbols extracted
  from its `toolchain-llvm-18.tar.gz`.
- `[DOCS]` — confirmed from an upstream source (URL cited).
- `[SPECULATION]` — inference not directly sourced; flagged as such.

---

## 1. Gnm / gnmx command-buffer builder library

**Does OpenOrbis ship an open-source reimplementation of the Gnm/gnmx PM4-emitting
builder library? — No, not in the toolchain itself.**

`[LOCAL]` The checkout has **no gnmx/Gnm builder library**. The only Gnm-related files:

- `include/orbis/GnmDriver.h` — declarations of the `libSceGnmDriver` entry points
  (submit + a handful of header-level PM4 packet builders like `sceGnmDrawIndexAuto`,
  `sceGnmSetEmbeddedVsShader`). These are the *driver* surface, not a gnmx builder.
- `include/orbis/_types/gnm.h` — a single 4-byte `OrbisGnmDrawFlags` union. Nothing else.
- `lib/` is **empty** except a placeholder README ("Generated library stubs will go
  here"). Stubs are unpacked from `toolchain-llvm-18.tar.gz` at install time.
- **No SDK source uses `sceGnm*`** — grep across `src/`, `samples/`, `extra/` matches
  only `GnmDriver.h` itself.

So the header exposes a *thin* set of packet builders (each `sceGnmDraw*` /
`sceGnmSet*Shader` writes a fixed number of PM4 dwords into a caller-supplied `cmd`
buffer — see §2), but there is **no gnmx-level "build a whole DrawIndexAuto pipeline,
set render targets, bind shaders" convenience layer** in-tree. A homebrew wanting a real
draw must hand-assemble the PM4 context/register setup itself around these builders.

`[DOCS]` The open-source builder *does* exist, but **out-of-tree**. OpenOrbis issue
[#5 "GNM Command Driver"](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/issues/5)
is an open "Future" enhancement (milestone V0.7) — "We need to write a driver for gnm
commands if we want to be able to fully use gnm for 3d graphics." Its comments point to a
third-party project by **pipehuffer / "veiledmerc"**:

- **`freegnm`** — a GNM-like GPU command/resource library, MIT, "work in progress and
  incomplete," built from "code from Mesa and AMD's PAL, including AddrLib and PM4
  definitions"; ships `pm4-dis` (PM4 disassembler) and `psb-dis` (PSSL shader-binary
  disassembler). <https://gitgud.io/veiledmerc/freegnm>
  ([README](https://gitgud.io/veiledmerc/freegnm/-/raw/master/README.md))
- **`freegnm-examples`** — <https://gitgud.io/veiledmerc/freegnm-examples>
- **`psbc`** — pipehuffer's words: a "Mesa based shader binary compiler."
  <https://gitgud.io/veiledmerc/psbc>

**freegnm is the closest thing to an open gnmx** and is the natural reference/corpus for
real Gnm draws (see §4). It is *not* wired into the OpenOrbis toolchain — pulling it in is
a maintainer decision.

Is the builder statically linked and does it call `libSceGnmDriver` submits via NID
imports (our HLE interception point)? `[DOCS]/[SPECULATION]` Yes — this matches doc-2 §3
and GPCS4's stated architecture: Gnm/gnmx builders (freegnm, or Sony's in retail games)
are compiled *into* the ELF and emit PM4 into buffers; the only dynamic-linking boundary
that crosses out is `libSceGnmDriver`'s submit functions. Those are exactly the NID
imports we intercept.

---

## 2. libSceGnmDriver stubs / NIDs

`[LOCAL]` `libSceGnmDriver.so` **is** shipped in the toolchain (extracted from
`toolchain-llvm-18.tar.gz`, 39.5 KB). It exports **202 `FUNC` dynamic symbols with plain
C names** (e.g. `sceGnmSubmitCommandBuffers`), not NID-mangled — the plain→NID mapping
happens at OELF/link time, not in this stub. The stub is link-time-only (bodies are
trap/placeholder); real behavior is the emulator's to provide.

`[LOCAL]` The submit/draw/shader entry points a graphics homebrew actually imports —
**all confirmed present in the stub AND declared in `GnmDriver.h`**:

| Purpose | Function | Notes (from `GnmDriver.h`) |
|---|---|---|
| Submit draw+compute CBs | `sceGnmSubmitCommandBuffers` | `(count, dcbaddrs[], dcbbytesizes, ccbaddrs[], ccbbytesizes)` |
| Submit + flip | `sceGnmSubmitAndFlipCommandBuffers` | adds `videohandle, displaybufidx, flipmode, fliparg` — **routes into videoout flip path** |
| End-of-frame | `sceGnmSubmitDone` | `(void)` |
| Flip + done | `sceGnmRequestFlipAndSubmitDone` | |
| Gate | `sceGnmAreSubmitsAllowed` | returns 1/0 |
| Draw (auto idx) | `sceGnmDrawIndexAuto` | writes **7** PM4 dwords into `cmd` |
| Draw (indexed) | `sceGnmDrawIndex` | writes **10** dwords; takes `indexaddr` |
| Default HW state | `sceGnmDrawInitDefaultHardwareState350` | writes **256** dwords of default context/register state |
| Set VS | `sceGnmSetVsShader` | **29** dwords; takes `vsregs` (shader register block) |
| Set PS | `sceGnmSetPsShader350` | **40** dwords; takes `psregs` |
| **Embedded VS** | `sceGnmSetEmbeddedVsShader` | **29** dwords; `shaderid 0 = fullscreen quad` |
| **Embedded PS** | `sceGnmSetEmbeddedPsShader` | **40** dwords; `shaderid 0 = empty`, `1 = empty exporting 32-bit R and G` |

Also present: `...ForWorkload` submit variants, `sceGnmInsertWaitFlipDone`, the marker
inserters (`sceGnmInsertSetMarker`, `...PushMarker`, …), `sceGnmSetCs/Es/GsShader`, the
indirect/multi draw family, and `initLibrary`.

**Interception model** `[LOCAL]/[DOCS]`: the builders (`sceGnmDraw*`, `sceGnmSet*Shader`)
run *inside* the guest and just emit PM4 dwords into guest memory — the emulator need not
implement them if the homebrew links them statically. The interception point is the
**submit family** (`sceGnmSubmitCommandBuffers`, `sceGnmSubmitAndFlipCommandBuffers`,
`sceGnmSubmitDone`), which is where guest-memory PM4 buffers cross into the driver. This
is the doc-2 §3 / decision-3 "hook GnmDriver submits, parse PM4" surface, and task-20's
stub target.

---

## 3. Shaders — the crux

### 3.1 Is there a shader compiler in the toolchain?

`[LOCAL]` **No.** `bin/{linux,macos,windows}` contain only packaging tools — `PkgTool.Core`,
`create-fself`, `create-gp4`, `readoelf`, `PkgEditor`. There is **no `orbis-wave-psslc`,
no GCN assembler, no shader compiler of any kind**, and no `.sb`/`.pssl` files anywhere in
`~/src/ps4labs`. The toolchain tarball likewise has no shader compiler.

`[DOCS]` Sony's `orbis-wave-psslc` (compiles `.pssl` → `.sb` via profiles like
`sce_vs_vs_orbis` / `sce_ps_orbis`) is a proprietary dev-machine-only tool and is **not
redistributed** with OpenOrbis
([GDC "PlayStation Shading Language for PS4"](https://gdcvault.com/play/1019252/PlayStation-Shading-Language-for),
[PSX-Place GameMaker PS4 notes](https://www.psx-place.com/resources/gamemaker-studio-gms.762/)).

`[DOCS]` **Open-source alternative: `psbc`** — pipehuffer's "Mesa based shader binary
compiler" (<https://gitgud.io/veiledmerc/psbc>). It takes **GLSL** and emits PS4 GCN `.sb`
binaries via Mesa's AMD backend — a GLSL→GCN path that sidesteps Sony's toolchain
entirely. This is what freegnm-examples uses (§4).

### 3.2 Precompiled GCN, and the "firmware stripped the runtime compiler" claim

`[DOCS]` Firmly supported: **PS4 games/homebrew ship precompiled GCN `.sb` blobs offline;
Gnm consumes already-compiled shader binaries** — there is no general-purpose runtime
PSSL→GCN compiler exposed to Gnm homebrew. Shaders reach the emulator as **GCN machine
code, never source** (doc-2's central finding, re-confirmed here).

`[DOCS]/[SPECULATION]` The *strong* form doc-2 stated — "retail firmware stripped the
runtime shader compiler" — the web pass could **not pin to a single authoritative
citation**. What is firmly documented is the weaker, sufficient version: Gnm requires
offline-precompiled `.sb`, homebrew ships them precompiled, and the one runtime-ish path
on retail is Piglet/WebKit's GLES stack backed by firmware-resident `ScePrecompiledShaders`
(§5). doc-2 cites flatz's OpenGL-ES write-up
([psx-place](https://www.psx-place.com/threads/opengl-es-implementation-on-the-ps4-write-up-provided-by-flatz.21600/))
for `libScePigletv2VSH`'s compiler being removed on retail; treat "the general PSSL
runtime compiler is stripped" as **plausible-but-not-cleanly-sourced**, "homebrew must
ship precompiled `.sb`" as **confirmed**. Practical consequence is identical either way.

### 3.3 The .sb (Sony shader binary) format

`[DOCS]` Confirmed from GPCS4's
[`GcnShaderBinary.h`](https://raw.githubusercontent.com/Inori/GPCS4/master/GPCS4/Graphics/Gcn/GcnShaderBinary.h)
(fpPS4 equivalent: [`chip/ps4_shader.pas`](https://github.com/red-prig/fpPS4/blob/trunk/chip/ps4_shader.pas)):

- **Magic `"OrbShdr"`** (7 bytes) in `ShaderBinaryInfo::m_signature` + version byte.
- Body is **raw GCN ISA machine code wrapped with a `ShaderBinaryInfo` header + semantic
  metadata tables.** Header fields: `m_type` (stage: pixel/vertex/compute/geometry/hull/
  domain), `m_pssl_or_cg`, `m_length` (24-bit GCN code size), `m_shaderHash0/1`,
  `m_crc32`, SRT flags (`m_isSrt`, `m_isExtendedUsageInfo`, …).
- Semantic metadata: `VertexInputSemantic` / `VertexExportSemantic` (VGPR↔semantic),
  `PixelInputSemantic` (interpolation modes incl. F16), `PixelSemanticMapping`
  (VS-output → PS-input linkage).

This is the parser unemups4 would need before any GCN recompiler/interpreter can run a
guest shader (the metadata drives vertex-attribute fetch and VS→PS varying linkage).
Format index: [psdevwiki File Formats](https://www.psdevwiki.com/ps4/Template:File_Formats).

### 3.4 The shader-blob-FREE draw path (embedded shaders) — the key finding

`[LOCAL]` `GnmDriver.h` documents **firmware-embedded shaders selected by ID that need no
`.sb` blob**:

- `sceGnmSetEmbeddedVsShader(cmd, 29, shaderid, shadermodifier)` — **`shaderid 0` =
  fullscreen-quad vertex shader**.
- `sceGnmSetEmbeddedPsShader(cmd, 40, shaderid)` — **`shaderid 0` = empty PS; `shaderid 1`
  = empty PS exporting 32-bit R and G**.

`[DOCS]` These match the standard Gnm `kEmbeddedVsShaderFullScreen` / embedded-PS
conventions in reverse-engineered Gnm docs and emulators.

**Verdict:** a homebrew *can* issue a real PM4 GPU draw with **no user-supplied `.sb`** by
doing: embedded VS id 0 (fullscreen quad) + embedded PS id 1 (export R/G) +
`sceGnmDrawIndexAuto` (+ default-HW-state + render-target setup). Output is constrained to
what the embedded PS exports — an R/G fullscreen fill, not an arbitrary-colored/textured
triangle — but it is a genuine GPU draw driven entirely by firmware-embedded shaders. To
*emulate* it, unemups4 must recognize the embedded-shader IDs in the PM4 stream and
synthesize a host fullscreen-quad VS + R/G-export PS itself (it will never see a guest
`.sb`). This is the **minimal, most emulator-friendly first GPU draw**.

### 3.5 Minimum path to a working VS+PS pair for a triangle

Three realistic routes, cheapest first:

1. **No blob — firmware-embedded shaders** (§3.4). Zero corpus; emulator synthesizes host
   shaders from the ID. Output limited (R/G fill).
2. **Compile your own with open `psbc`** (GLSL→`.sb`) — what freegnm-examples/triangle
   does. No Sony toolchain. Produces real `OrbShdr` blobs → needs a full `.sb` parser +
   GCN recompiler to run.
3. **Extract/dump `.sb`** from an existing sample or Sony WebKit/GLES precompiled packs,
   or hand-assemble GCN. Dumping tooling exists
   ([PSSL pre-compiled shader dumper](https://www.psxhax.com/threads/ps4-opengl-pssl-pre-compiled-shader-dumper-by-theorywrong.6590/)).

---

## 4. Available Gnm samples (candidate corpus)

`[LOCAL]` **In the OpenOrbis toolchain: no raw-Gnm/PM4 triangle sample.** The `samples/`
graphics options are:

- **`samples/graphics`** — CPU **Mandelbrot to framebuffer via SceVideoOut**. Links only
  `-lc -lkernel -lc++ -lSceVideoOut`. **No gnm, no shaders.** This is exactly the videoout
  path unemups4 already runs — *not* a GPU-draw path.
- **`samples/piglet`** — OpenGL ES 2.0 / EGL 1.4 via Sony **Piglet** (§5). Links
  `-lScePigletv2VSH -lScePrecompiledShaders -lSceShellCoreUtil -lSceSysmodule …`. Pulls in
  the whole GLES/Piglet system-library stack; relies on firmware-resident precompiled
  shaders. **Not a raw-Gnm sample.**

`[DOCS]` **Out-of-tree: `freegnm-examples` is a working raw-Gnm corpus.** Tree:
`triangle, cube, gltf, indirect, instances, primitives, shared`
([tree](https://gitgud.io/api/v4/projects/veiledmerc%2Ffreegnm-examples/repository/tree),
UNLICENSE). The **`triangle`** example ships **GLSL source**
(`triangle/assets/misc/tri.vert.glsl`, `tri.frag.glsl`, `clear.frag.glsl`) compiled to
GCN `.sb` by **psbc** and submitted via freegnm's PM4 builder with `DrawIndexAuto`. At
runtime it needs: the freegnm builder linked in, the two compiled `.sb` blobs, and a
`libSceGnmDriver` submit path — i.e. it exercises the *full* PM4+GCN pipeline, not just
present/sync.

---

## 5. Piglet / PGL (context)

`[DOCS]` Piglet (PGL, `libScePigletv2VSH`) is **Sony's OpenGL ES 2.0 + EGL 1.4
implementation layered over Gnm**, originally for the PS4 WebKit browser/UI. OpenOrbis'
`piglet` sample links `-lScePigletv2VSH -lScePrecompiledShaders` and credits "orbisdev and
flat_z for their Piglet RE effort"; `ScePrecompiledShaders` are Sony's firmware-resident
shader set "used by Sony in WebKit"
([samples/piglet/README.md](https://raw.githubusercontent.com/OpenOrbis/OpenOrbis-PS4-Toolchain/master/samples/piglet/README.md)).
Piglet gives homebrew a GLES2 API **without hand-writing Gnm/PM4 or supplying GCN `.sb`**,
because it leans on `ScePrecompiledShaders` rather than a runtime PSSL compiler — which is
why most OpenOrbis GPU homebrew uses it (e.g.
[OrbisGL](https://github.com/marcussacana/OrbisGL),
[orbisGlPerf](https://github.com/orbisdev/orbisGlPerf)). For unemups4 this path is
**expensive** (emulating the Piglet+GLES2 system libraries and the precompiled-shader set)
and is not the phase-3/4 target — raw Gnm/PM4 is.

---

## 6. How shadPS4 / fpPS4 / GPCS4 handle guest GCN shaders

`[DOCS]` All three **parse raw PM4 and recompile guest GCN ISA to host shaders at
runtime** (recompile, not interpret):

- **shadPS4** — GCN bytecode → IR → **SPIR-V** for Vulkan (custom "Hades"-style
  recompiler), with a pipeline cache; handles GCN fetch shaders
  ([Pipeline Cache & Compilation](https://deepwiki.com/shadps4-emu/shadPS4/4.5-pipeline-cache-and-compilation),
  [Graphics System](https://deepwiki.com/shadps4-emu/shadPS4/4-graphics-system)).
- **GPCS4** — parse PM4 → recover Gnm calls → GCN → SPIR-V
  ([GraphicsStack.md](https://github.com/Inori/GPCS4/blob/master/Doc/GraphicsStack.md)).
- **fpPS4** — PM4 + GCN → SPIR-V; shader parsing in
  [`chip/ps4_shader.pas`](https://github.com/red-prig/fpPS4/blob/trunk/chip/ps4_shader.pas).

decision-3 deliberately diverges here: unemups4's phase-4 plans a **CPU GCN interpreter
first** (correctness oracle) before any recompiler. Nothing in the local toolchain
changes that; these emulators are the reference for the *later* recompiler phase, and
their `.sb`/GCN parsers (§3.3) are directly reusable as spec references.

---

## 7. Implications for task-22 and phase-3/4

### task-22 (hand-written PM4 test ELF) — can it proceed shader-free?

**Yes — and it should.** task-22 is already scoped (correctly) as a hand-written PM4 ELF
because OpenOrbis ships no native Gnm sample (task-22 desc / doc-2 §2). This research
confirms and sharpens that:

- The `libSceGnmDriver` stub with all needed submit/draw NIDs **is present locally**
  (§2) — task-20 stubbing has a concrete symbol list to target, and task-22 can call the
  builders/submit exactly as its Makefile pattern expects.
- task-22's acceptance criteria (issue a DrawIndexAuto-class PM4 stream; produce a PM4
  trace via task-21) need **no shaders at all**. A **clear + present-only PM4 stream** —
  `DrawInitDefaultHardwareState350` (256-dword default state) → a clear via
  render-target setup → `SubmitAndFlipCommandBuffers` — is a valid, decoder-exercising
  first corpus and matches phase-2 "boot-and-trace" (D1) + phase-3 "present/sync" (D2)
  before any shader work.
- If task-22 wants an actual **draw** packet in the stream (AC #2 says "DrawIndexAuto-class"),
  it can use the **embedded-shader path** (§3.4): `sceGnmSetEmbeddedVsShader(0)` +
  `sceGnmSetEmbeddedPsShader(1)` + `sceGnmDrawIndexAuto` — still **no `.sb` blob**, still
  fully hand-written, and it gives phase-3 something to actually rasterize later
  (fullscreen R/G fill) without pulling in a GCN recompiler.

**Verdict: task-22 proceeds shader-free.** Recommended: build the corpus in two tiers in
one ELF (or two examples) — (a) `initDefaultHardwareState → clear → SubmitAndFlip` for the
pure trace/present milestone; (b) add `SetEmbeddedVs(0)+SetEmbeddedPs(1)+DrawIndexAuto` for
the first embedded-shader draw. Neither needs a shader compiler or any blob.

### First real GCN-shader milestone — corpus

Two stops, in order:

1. **Embedded-shader draw (no corpus/no blob).** Emulate the embedded VS/PS IDs by
   synthesizing host equivalents (fullscreen quad VS, R/G-export PS). This is the *first*
   thing that puts guest-driven geometry on screen and needs **zero** GCN decoding. Do
   this before any `.sb` work.
2. **First arbitrary VS+PS triangle: `freegnm-examples/triangle`.** GLSL → `psbc` →
   two `OrbShdr` `.sb` blobs → freegnm PM4 builder → `DrawIndexAuto`. Running this requires
   the `.sb` parser (§3.3) **and** a GCN-ISA→host shader step (interpreter first per
   decision-3, recompiler later) — the large, unavoidable piece. Its GLSL sources are the
   reference for what a minimal VS+PS must do.

### Toolchain gaps the maintainer must fill

- **No shader compiler is installed** `[LOCAL]`. For milestone (1) none is needed
  (embedded shaders). For milestone (2), obtaining `.sb` blobs requires either building
  **`psbc`** (Mesa-based; a real build effort) or dumping blobs — a **deliberate maintainer
  decision**, not something the toolchain provides.
- **No native Gnm sample / no gnmx builder in-tree** `[LOCAL]`. task-22's hand-written ELF
  is the answer for the first corpus; **`freegnm` + `freegnm-examples`** (out-of-tree,
  MIT/UNLICENSE) is the answer for the first *arbitrary-shader* corpus if/when the
  maintainer chooses to vendor or reference them.
- No `.sb` parser or GCN decoder exists anywhere in Rust `[DOCS]` (doc-2 §4) — both are
  from-scratch work for phase-4, informed by GPCS4/fpPS4 `.sb` layouts and the SI ISA
  manual.

---

## 8. Local toolchain inventory summary

`[LOCAL]`, from `~/src/ps4labs/ps4sdk` (`git remote` = OpenOrbis-PS4-Toolchain):

| Item | Present? | Detail |
|---|---|---|
| gnmx / Gnm PM4 **builder** library | ✗ | not in-tree; open impl is out-of-tree `freegnm` |
| `GnmDriver.h` (driver decls) | ✓ | submit + thin packet builders + embedded-shader decls |
| `libSceGnmDriver.so` stub | ✓ | 202 plain-named FUNC symbols; all submit/draw/embedded-shader entries |
| Shader compiler (psslc / GCN asm) | ✗ | only packaging tools in `bin/` |
| `.sb` / `.pssl` shader blobs | ✗ | none anywhere in the checkout |
| Native raw-Gnm/PM4 sample | ✗ | `samples/graphics` = CPU Mandelbrot; `samples/piglet` = GLES2 |
| EGL/GLES2 headers (piglet path) | ✓ | `include/{EGL,GLES2,KHR}` |
| Packaging tools | ✓ | `PkgTool.Core`, `create-fself`, `create-gp4`, `readoelf`, `PkgEditor` |

---

## 9. Sources

Local: `~/src/ps4labs/ps4sdk` (OpenOrbis-PS4-Toolchain checkout) — `include/orbis/GnmDriver.h`,
`include/orbis/_types/gnm.h`, `samples/graphics/{Makefile,README.md}`,
`samples/piglet/Makefile`, `lib/`, `bin/`, and `lib/libSceGnmDriver.so` extracted from
`toolchain-llvm-18.tar.gz`.

Web:
[OpenOrbis toolchain](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain) ·
[GNM Command Driver issue #5](https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/issues/5) ·
[GnmDriver.h (upstream)](https://raw.githubusercontent.com/OpenOrbis/OpenOrbis-PS4-Toolchain/master/include/orbis/GnmDriver.h) ·
[samples/graphics/README.md](https://raw.githubusercontent.com/OpenOrbis/OpenOrbis-PS4-Toolchain/master/samples/graphics/README.md) ·
[samples/piglet/README.md](https://raw.githubusercontent.com/OpenOrbis/OpenOrbis-PS4-Toolchain/master/samples/piglet/README.md) ·
[freegnm](https://gitgud.io/veiledmerc/freegnm) ·
[freegnm-examples](https://gitgud.io/veiledmerc/freegnm-examples) ·
[psbc](https://gitgud.io/veiledmerc/psbc) ·
[GPCS4 GcnShaderBinary.h (.sb format)](https://raw.githubusercontent.com/Inori/GPCS4/master/GPCS4/Graphics/Gcn/GcnShaderBinary.h) ·
[GPCS4 GraphicsStack.md](https://github.com/Inori/GPCS4/blob/master/Doc/GraphicsStack.md) ·
[fpPS4 ps4_shader.pas](https://github.com/red-prig/fpPS4/blob/trunk/chip/ps4_shader.pas) ·
[shadPS4 pipeline cache/compilation](https://deepwiki.com/shadps4-emu/shadPS4/4.5-pipeline-cache-and-compilation) ·
[shadPS4 graphics system](https://deepwiki.com/shadps4-emu/shadPS4/4-graphics-system) ·
[psdevwiki File Formats](https://www.psdevwiki.com/ps4/Template:File_Formats) ·
[GDC PSSL for PS4](https://gdcvault.com/play/1019252/PlayStation-Shading-Language-for) ·
[flatz OpenGL ES write-up](https://www.psx-place.com/threads/opengl-es-implementation-on-the-ps4-write-up-provided-by-flatz.21600/) ·
[PSSL pre-compiled shader dumper](https://www.psxhax.com/threads/ps4-opengl-pssl-pre-compiled-shader-dumper-by-theorywrong.6590/) ·
[OrbisGL](https://github.com/marcussacana/OrbisGL) ·
[orbisGlPerf](https://github.com/orbisdev/orbisGlPerf)
