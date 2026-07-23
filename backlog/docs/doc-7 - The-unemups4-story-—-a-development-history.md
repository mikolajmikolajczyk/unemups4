---
id: doc-7
title: The unemups4 story — a development history
type: other
created_date: '2026-07-23 10:46'
---

# The unemups4 story — a development history

This is the human-readable history of how unemups4 was built: the arc, the
breakthroughs, the setbacks, the dead ends, and which piece of work pushed the
project forward or knocked it over. The git history is periodically re-rooted, so
this document is where the story is kept.

It is a running record — append to it as new chapters happen. The engineering
detail lives elsewhere: the bring-up *method* in [doc-4], the worked *debugging
cases* in [doc-5], the GPU reverse-engineering *discovery log* in [doc-6]. This is
the narrative that ties them together.

A note on how the project works, because it shaped everything below: every step is
gated on something **visibly working** — a homebrew that boots, a triangle that
renders, a title that reaches its menu — and the maintainer's own eyes are the
final oracle for anything on screen. Progress is measured in walls cleared, not
features added.

---

## Act 1 — Foundations: a controlled CPU and a flat address space

The first real commitment was the execution model. Guest code runs on the
**x86jit** x86-64 engine (interpreter first, JIT for speed) over an
**identity-mapped** address space — guest addresses equal host addresses — so a
pointer in a command buffer or a shader binary can be read straight out of guest
memory with no translation layer. Library imports resolve to `SYSCALL` stubs that
trap out into Rust HLE handlers.

That choice bought control (no native jumping around the host), portability
(ARM64/Metal down the line), and honest diagnostics — and it set the pattern the
whole project would reuse: **an interpreter as the correctness oracle, the JIT
chasing speed, differential tests comparing the two.** Threads, TLS destructors,
and nested guest calls were brought up and validated on this engine early, before
anything visual existed.

None of this is the fast path, and that is a deliberate choice. A JIT-over-
interpreter design carries real overhead against running guest code natively on the
host — and unemups4 pays it on purpose. This is, first and foremost, the author's
project for **fun and for learning**: there is little point building the *n*-th
native-execution PS4 emulator, and there is no ambition here to be a *fast* one. The
goal is to understand each piece and to **write down how it actually works** — how
the reverse-engineering was done, why each decision was made, what each wall taught
— so the emulator is as much a record of the process as a program that runs games.
Speed is a nice-to-have; the explanation is the point.

The x86jit engine is a separate library with its own backlog; unemups4 pins it at a
fixed revision. That seam mattered later — a huge amount of retail bring-up turned
out to be *"the guest executed an instruction the engine didn't lift yet."*

## Act 2 — The GPU spine, and the keystone

The graphics stack is the spine of the project. It came up in deliberate phases,
each independently demonstrable:

- **PM4 present/sync executor** — the guest submits command buffers full of PM4
  packets; the executor walks them, turning `SubmitAndFlip` into a present and
  end-of-pipe events into completion labels.
- **Loader** — resolve the guest's NID-hashed imports; load `.prx` modules.
- **GCN decoder + disassembler** — read the AMD GCN shader ISA the guest ships as
  precompiled machine code.
- **GCN → SPIR-V recompiler** — translate those shaders to SPIR-V for Vulkan,
  kept portable (MoltenVK-safe) from the first line. This took two hardening
  passes to get the machine-generated, unstructured control flow through cleanly.

Then the **keystone (task-53): a real GCN `.sb` draw, end-to-end.** Resolve the
shader, feed cache-backed vertex/index buffers, emit the draw — and a
**triangle rendered on screen**, verified by a PNG oracle. That was the moment the
whole phased bet paid off: the pieces converged into a picture. A
register-route triangle homebrew (task-96) proved it wasn't a fluke.

Close behind came the **textured-draw milestone (task-55)**: `image_sample`, T#/S#
descriptor decode, and a texture cache — a textured quad drawn from real
descriptors. The GPU could now do the two things every real title needs: draw
geometry, and sample textures.

## Act 3 — A Doom port: the first full program

Homebrew triangles are one thing; a full program is another. The first was a
**PS4 Doom port the author wrote** (`ps4doom`, unpublished) — Doom is open source,
so porting it gave the emulator a substantial, real target that was also entirely
under the author's control: when something broke, both sides of the boundary could
be inspected. It needed filesystem-mutation syscalls (mkdir/unlink/rename) to get to
its title screen, a physical-gamepad + keyboard input path to become **playable**,
a `flipArg` fix to stop throttling the guest, and an audio HLE path — after which
the Doom audio build ran at **62 fps with sound**. (The first *retail* — commercial,
not-our-code — title comes in Act 4.)

**First setback worth remembering:** an x86jit revision bump meant to add an
instruction *froze Doom*. It was caught and **rolled back** the same day. The
lesson stuck — a dependency bump is measured against the working title before it
stays, never assumed.

## Act 4 — Retail: the long war to boot Celeste

Celeste is a managed-runtime title (a Mono/MonoGame/FMOD stack), and bringing it up
was a war of attrition fought one wall at a time. This is where the **smoke-loop
method** ([doc-4]) and the **debugging casebook** ([doc-5]) were born — because the
walls were coming fast enough to need a system.

The arc: call `.prx` `module_start` leaves-first so the runtime initializes in
order; net/scheduler/`getcwd` stubs so **Mono loads `Celeste.exe`**; build the
Orbis argument block for the guest entry; platform/audio/savedata init so the guest
**reaches graphics init**; a GNM front-door plus shader emitters so **Celeste binds
real shaders and records draws**.

Threaded through all of it was **the instruction-lift saga** — the single most
repetitive struggle in the project. Celeste's runtime executes a long tail of
SSE/AVX instructions, and each missing one was a hard stop: `MOVMSKPS`, `VROUNDSD`,
pack/unpack, `hadd/hsub`, `phadd`, `vpsadbw`, `CMPSS`, `vmovlhps`, `movddup`, and
more. Each was a diagnosis ("the guest trapped on this opcode"), a task filed to the
x86jit backlog, a landing, and a pin bump. Wall after wall after wall — unglamorous,
and exactly the work that got the title running.

## Act 5 — Celeste on screen: the pixel war

Booting is not rendering. Getting Celeste to actually *look right* was its own
campaign, and it produced the project's most important diagnostic habit: **when the
picture is wrong, prove where the fault is before fixing anything.**

Turning points and hunts:

- **The attract-loop keystone (task-170):** Celeste sat forever on its attract
  screen. The cause was a wrong SDK status — a "not logged in" state reported as
  "no event" — that never let the title advance to its menu. A one-value fix
  unblocked the whole front end.
- **The real-hardware oracle.** A capture of the real console's own GPU command
  stream became the ground truth. It first *proved the splash loop faithful* (the
  emulator was doing the right thing), then settled a much harder case:
- **Atlas-splatter — the emulator exonerated three times.** Celeste's sprites were
  garbled, and the obvious suspect was our GPU. The console oracle said otherwise:
  the divergence was **guest-side** — the game's own sprite-batch vertex data — and
  the emulator's GPU was cleared three separate ways (differential JIT-vs-interp,
  the buffer-content oracle, a metadata trace). A genuine dead end for the "fix the
  GPU" instinct, and a lesson: the emulator is not guilty by default.
- **The palette, the fills, the menu.** A warm-palette bug fixed by honoring the
  real videoout format; the guest's full-screen fills (a rectangle-list primitive
  with no Vulkan equivalent) drawing again so **the menu renders**; then gameplay
  entry once POSIX failures set the guest's `errno` correctly, analog sticks
  centered so the menu stops scrolling, and saves persisting across reloads.
- **In-game scene renders correctly** once the pixel shader could bind *multiple*
  textures (it had been collapsing every sample onto one), a few missing GCN ops
  were added, and sampler state was honored — the yellow-sky and distortion passes
  finally composited right.

## Act 6 — Two titles, one signal, and the shape of the wall ahead

With Celeste playable, two fronts opened.

**Performance.** Decoding PM4 in place, not double-buffering the swapchain, and
actually *measuring* the frame lifted the **menu** to ~58 fps. **Gameplay** is a
different story — it sits at roughly **24–26 fps** (peaking around 32) — and the
measurement told the uncomfortable truth about why: the remaining cost is **guest
CPU** — 129–145 MIPS of interpreted guest code, dominated by a write barrier that
was **250× more expensive than everything else**. The gap is per-instruction cost, not
workload — the honest signal that the JIT, not the GPU, is the next frontier. (One
diagnostic, task-220, is filed with a candid note that it *cannot answer the
question it was filed for* — kept as a marker of a measurement dead end.)

**A second retail title (Little Nightmares, an Unreal Engine 4 game).** It boots
through hundreds of imports once the network stack answers *as a console with no
link* rather than as one whose libraries are broken, and once the loader names the
library an unresolved import came from and refuses an unresolvable dependency cycle
cleanly. It then renders a handful of frames and stalls on a **multi-threaded RHI
deadlock** — general cross-thread events the kernel doesn't model yet (task-230).
That is the current far wall.

The most interesting engineering knot of this era was the **EOP completion label**:
one title collapses if you write it, another deadlocks if you don't, and the two are
indistinguishable at the moment you must decide. The resolution generalizes: don't
look for a switch you can read *now* — default to the choice that can't hurt the
working title, and switch only on **positive proof that accrues over time**. That
principle, and the stall-diagnosis toolbox built alongside it, are in [doc-6].

## Where things stand, and the north star

Today: **Celeste boots to gameplay with input, correct palette, and correct
textures** — the menu runs at ~58 fps, gameplay at ~24–26 (the guest-CPU wall from
Act 6). Doom is playable with audio. A second retail title reaches
first frames before its RHI wall. The GPU path is real (PM4 → GCN decode →
interp/recompiler → Vulkan), kept portable toward MoltenVK/Metal.

Every hardware and OS fact in the tree is derived from and cited to a **clean
primary source** — the AMD GCN ISA, Mesa, the Linux kernel AMD headers, the
OpenOrbis SDK, FreeBSD, and the real PS4 console capture — and pinned with a witness
test, so a value that drifts from the hardware fails the build. The console capture,
decoded through the emulator's own PM4 decoder (`dcbdump`), is the ground-truth
oracle: the surest way to know the emulator is right is to reproduce what the
console itself emits.

The north star remains far off and deliberately so: **run Bloodborne**, a multi-year
target that pulls the CPU (a faster JIT), the GPU (broader GCN and PM4 coverage),
and the kernel (cross-thread events, more of libkernel) forward together. The
project stays honest about the tension between that ambition and its
lightweight-educational ethos by keeping every phase independently useful — stall
before the north star and you still have a working, demonstrable emulator.

The pattern that got this far is the one that continues it: clear the smallest wall
in front of you, prove the fix on something you can see, and write the lesson down.

[doc-4]: <doc-4 - Retail-title-bring-up-—-the-smoke-loop-method.md>
[doc-5]: <doc-5 - Retail-bring-up-casebook-—-worked-debugging-examples.md>
[doc-6]: <doc-6 - Retail-GNM-bring-up-—-discovery-log-how-the-GPU-path-was-reverse-engineered.md>
