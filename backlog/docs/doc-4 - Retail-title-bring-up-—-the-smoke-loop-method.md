---
id: doc-4
title: Retail title bring-up — the smoke-loop method
type: other
created_date: '2026-07-15 04:35'
---

# Retail title bring-up — the smoke-loop method

**Audience.** You have an HLE PS4 emulator that runs simple homebrew, and you want to boot a
real retail title (a large, unfamiliar binary you did not write). You do not know where to
start. This document is the method: how to make forward progress *deterministically* on a
black-box binary, how to see **where** and **why** it stops, and how to decide what to do
about it. It is written from the notes of an actual bring-up (a Mono/MonoGame title, from
"crashes on the first instruction" to "the managed runtime loads the game assembly").

The companion doc-5 (*casebook*) walks concrete worked examples. Read this first for the
method, then doc-5 for the pattern-matching.

> Nothing here is title-specific magic. It is a loop and a toolbox. The title is a black
> box that emits *symptoms*; your job is to turn each symptom into a precise cause, fix the
> smallest thing, and rerun.

---

## 1. The mental model

Know exactly what is executing and where the seams are, or you will misread every symptom.

- **Guest code runs for real** on the CPU engine (here: x86jit — interpreter + JIT). It is
  the *title's own* x86-64 code: its CRT, its libc, its runtime (Mono), its game code.
- **The address space is identity-mapped**: guest address == host address inside a big
  reserved arena (`MAP_NORESERVE`, so untouched pages cost nothing). This is the single
  most important fact for debugging: a guest pointer *is* a host pointer. A bad guest
  pointer dereferenced by your emulator code segfaults the **host process**, not the guest.
- **Library calls trap out.** The title imports `sceKernel*`, `scePthread*`, libc, … by
  **NID** (an 11-char hash of the symbol name). The loader resolves each import to a small
  stub that executes `SYSCALL`, which the run loop catches as an exit and dispatches to a
  Rust **HLE handler**. Unresolved imports get a "missing symbol" stub that reports and
  stops.
- **You are emulating three things at once:** the CPU (instructions), the kernel/libraries
  (HLE handlers), and the loader/ABI (how the binary is mapped, linked, and entered). A
  wall belongs to exactly one of these. Naming which one is half the fix.

---

## 2. The core loop

```
build → run the title → it stops → characterize WHERE and WHY → fix the smallest thing → rerun
```

That is the whole method. The title makes progress one wall at a time. Each iteration you
learn one concrete fact about what the title needs. Do not try to predict the walls; let the
binary tell you. Keep the iterations small and always rerun to confirm the wall moved.

**Discipline that keeps this sane over dozens of iterations:**

- **Commit per wall** (or per small batch of related stubs), with a message saying which
  wall fell and how. When something regresses 20 walls later, `git bisect` is your friend.
- **Verify no regression each time.** Keep a couple of known-good homebrew/examples and a
  smoke run of the title; after every fix, confirm the examples still pass *and* the title's
  wall advanced. A "fix" that breaks an example is not a fix.
- **Anchor what you defer.** When you stub something shallow or spot a latent bug you are not
  fixing now, leave a `KNOWN LIMITATION (task-N)` comment at the exact code site and file the
  task. Six walls later you will not remember; the grep will.
- **One symptom, one cause.** Resist fixing "what looks wrong nearby." Fix the thing the
  current wall actually needs, rerun, and let the next wall tell you what is next.

---

## 3. The diagnostic toolbox — seeing *where* it stopped

You cannot fix what you cannot locate. Build/keep these instruments; each turns a class of
symptom into an address, a name, or a byte string.

### 3.1 The "why did it stop" reporter

Every fatal exit should print an *actionable* report, not just "it died". Minimum:

- **Missing symbol** → the raw NID **and** its human name (resolve the NID through your
  name map). `[FATAL] missing symbol: sceKernelFoo [NID abc...]` beats a bare hash — you
  can act on a name; you cannot act on a hash.
- **Unmapped memory** → the faulting RIP, the access kind (read/write/exec), the address,
  a disassembly of the faulting instruction, and the **nearest VMA** ("inside libc
  [range]"). Now you know which module faulted and on what.
- **Unknown instruction** → the RIP, the decoded mnemonic, and the **raw bytes** ready to
  paste into a CPU-backlog task.
- **Exception/trap** → the vector, a signal-style name, and (for `int`/`int3`) back up one
  byte to name the trapping instruction.

If your emulator does not already print these, adding them is the highest-leverage work you
can do before touching the title. You will read these reports hundreds of times.

### 3.2 The guest backtrace (walk the frame-pointer chain)

The single most useful diagnostic for a large title. On a fatal fault, walk the guest
`rbp` chain: `*rbp` is the caller's saved rbp, `*(rbp+8)` is its return address. Print a
dozen frames, attributing each return address to a module + offset via the VMA map:

```
guest backtrace (rbp chain):
  #0 ret 0x959bfd  [inside "libc" (offset +0x2dbfd)]
  #1 ret 0x957277  [inside "libc" (offset +0x2b277)]
  #4 ret 0x92c064  [inside "libc" (offset +0x64)]     ← module_start
  #5 ret 0x30000   [inside "hlt_gadget"]              ← your return gadget
```

Why it matters: a *deliberate* guest abort (an assert, a panic, `int 0x44`) leaves the RIP
on the trap instruction, which only names the panic helper — **not who called it**. The
backtrace gives you the caller chain, so you can find the function that *decided* to abort
and read its logic. It cracked more than one wall in this project (see doc-5 §3).

Depends on frame pointers existing; the PS4 CRT/libc keep them. It is best-effort; stop at
the first unreadable or non-increasing link.

### 3.3 Interpreter vs JIT — two lenses on the same fault

A JIT that identity-maps guest memory will pass a bad guest access straight to the host and
**SIGSEGV the host process** (you get `exit 139` / a core dump, *not* a clean guest-fault
report). The interpreter, checking each access, can surface the same access as a clean
`UnmappedMemory` guest fault with your backtrace attached.

So: **JIT for speed** (the default; the title runs in ~milliseconds up to the wall), but when
you get an opaque host crash, **rerun under the interpreter** for a clean, attributed guest
fault. It is slower but actionable.

### 3.4 gdb — for host crashes the interpreter can't reframe

Sometimes the crash is genuinely in *your* HLE code, not guest code — an HLE handler
dereferenced a bad guest pointer. Run the JIT build under gdb and let it catch the SIGSEGV:

```
gdb -q -batch -ex run -ex 'bt 12' --args ./emu path/to/eboot.bin
```

The host backtrace lands you *inside your handler* with the offending value in a register:

```
#1 core::ffi::c_str::strlen (ptr=0x44)                  ← reading host address 0x44
#4 sce_pthread_mutex_init (..., name=0x44)              ← the guest passed 0x44 as a "name"
```

That is a precise, unambiguous cause: your handler read a guest pointer without validating
it, the guest passed junk in that register, and the identity map turned it into a wild host
read. (Fix: range-check guest pointers before dereferencing — see §5.3.)

### 3.5 Disassembling the guest

When a fault is in the *title's* code and you need to understand the logic, disassemble
around the RIP:

1. **Get the inner ELF.** A SELF is a container; unwrap it to the plaintext ELF (parsing a
   container, not decryption — never touch crypto). Dump the bytes to a file.
2. **Map vaddr ↔ file offset.** For each `PT_LOAD`: `file_off = ph.p_offset + (vaddr - ph.p_vaddr)`.
   Do this for the segment that contains your RIP; the mapping is *not* uniform across
   segments (text and data have different `p_offset` deltas).
3. `objdump -D -b binary -m i386:x86-64 --start-address=… --stop-address=…` the window.
   Read backward from the fault to find where the faulting register was set.

**Watch for BSS.** If a global reads as 0 but the file bytes there look like something else,
check `p_filesz` vs `p_memsz`: an offset beyond `p_filesz` is zero-initialized BSS. A
BSS-zero global that the code dereferences means "some runtime init step should have set this
and didn't" — a different bug class than a missing relocation.

### 3.6 Relocations and NIDs

- **Resolve NIDs to names** end-to-end (import log, missing-symbol report, and when you
  disassemble a call through the PLT/GOT, resolve the GOT slot's relocation symbol back to a
  name). Most walls are "the title wants function X"; a name tells you what X does.
- **Read relocations directly** when a global is unexpectedly null. Is there a relocation for
  that offset? `Absolute64` against an *imported* symbol that never resolved is written as 0
  → the guest derefs null. That is how `__stack_chk_guard` (a *data* import) showed up as a
  null-canary crash (doc-5 §4).

### 3.7 In-flight syscalls — which thread is parked, and in what

The per-syscall profiler table counts calls that **returned**. A thread parked inside a
blocking call therefore never appears in it, which reads as "that thread makes no syscalls"
and is the exact opposite of the truth. When a title stops making progress, that thread is
the whole question.

`UNEMUPS4_PROFILE` now also prints, per guest thread, the syscall entered and not yet
returned, longest wait first:

```
in-flight syscalls: 3 thread(s) inside a call that has not returned
  tid 3    scePthreadCondWait    blocked 39.232 s
  tid 2    scePthreadCondWait    blocked 39.232 s
  tid 4    scePthreadCondWait    blocked 39.232 s
```

Read it together with the completed table. Three threads in `cond_wait` **and** no
`scePthreadCondSignal` anywhere in the completed calls means nothing ever signals them — an
idle worker pool, correctly parked, and *not* your deadlock. That combination ruled out a
deadlock reading in one dump.

Note `/proc/<pid>/task/*/wchan` still helps (§4) but stops at `futex_wait`; it cannot tell an
HLE condvar from a host lock. This does.

### 3.8 The import-stub map — naming an address that belongs to no symbol table

An emitted import stub is memory *we* wrote, so no module's symbol table covers it. A GOT slot
in a dumped module holds a bare address and the investigation stalls there.

Every stub is now registered as `addr -> "lib!symbol"`, both the HLE ones and the lazy
missing-symbol ones. The fault reporter resolves an address against that registry **before**
falling back to module symbols, and `UNEMUPS4_DUMP_MODULES` also writes `hle_stubs.map` for
offline work — which is where it matters, because the question usually starts at a GOT slot in
a dump.

This is the entry point for the walk used repeatedly below: **stub address → the GOT slot
holding it → the PLT entry jumping through it → the `call` sites**. A short Python scan over
the dumped image finds each step (`ff 25` for the PLT, `e8` for direct calls); note UE4 builds
often call indirectly, so check `ff 15` too, and the slot may live in a **sibling module**
rather than the eboot.

### 3.9 Logging syscall arguments

When a handler "succeeds" but the title still misbehaves, log the actual arguments the guest
passed. The `len=0` that revealed the `fstat`→`invalid CIL` chain (doc-5 §6) was invisible
until the mmap/read handlers logged their arguments. Add a temporary `info!` with the args,
rerun, read, remove it.

### 3.10 Diagnosing a silent stall — name who is stuck, not just that something is

A title that hangs with no fault (doc-5 case 31) gives you nothing to grep for. Three tools
turn "30 threads are parked" into a one-line diagnosis, each naming the input to the next.
None needs ptrace (usually blocked by Yama; a fault-time breakpoint munmaps the arena anyway):

- **Named in-flight syscalls.** The profiler's in-flight list (§3.7) carries each guest
  thread's *name* (`thread_name_of`), so it reads `SubmitDoneAsyncTaskThreadPS4`, not `tid 27`.
  The names come free from `pthread_create_name_np` / `scePthreadRename`. Sort so the
  longest-blocked waiter of *each distinct call* prints first — a stalled engine's list is one
  idle worker pool plus the one interesting thread stuck in a different call, and duration alone
  buries it under the pool.
- **The `[SYNC]` stuck-lock reporter.** When a guest mutex is held past a threshold (5 s), log
  *which* lock, *who* waits, *who holds*, and *how long*. This is the step that turns the victim
  (a thread in `scePthreadMutexLock`) into the culprit (the thread holding the lock it wants).
  Report once per waiter, not once per timeout tick — a permanent deadlock has nothing new to
  say after the first line.
- **Last syscall before the silence.** In exectrace, record each thread's most recent syscall
  and when. A gone-quiet thread's *last* call is the lead, and the per-thread histogram cannot
  show it — a histogram has no order. "tid 1: last syscall 42 s ago, `sceGnmSubmitCommandBuffers`"
  plus a 99%-one-address RIP histogram (§3.3) is "submitted, then spun waiting for it to finish".

The pattern: a stall is a graph of who-waits-on-whom. Build the tool that prints the edges
(holder ← waiter), follow them to the root, then read the root's last action and its spin RIP.

---

## 4. The taxonomy of walls

Every wall is one of a handful of shapes. Recognize the shape, apply the matching move.

| Symptom | Class | First move |
|---|---|---|
| `[FATAL] missing symbol: X [NID …]` | Missing HLE | Implement or stub X (§5.1) |
| `UnknownInstruction … : vfoo …` (bytes) | CPU lift gap | Delegate to the CPU engine (§6) |
| `exit 139` / core dump, no guest report | Host SIGSEGV | gdb → bad guest ptr in a handler (§5.3) |
| `exit 139`, **intermittent** (survives under `=debug`), gdb `bt` bottoms in a system `.so` (e.g. `libvulkan_radeon.so`) during pipeline/shader compile | **Driver** crash on *our* input, not our race | Don't assume the race is yours. `spirv-val` the module we fed it (validate the input); then **swap the library's internal backend** (`RADV_DEBUG=llvm`, or another ICD) — if a *valid* input stops crashing, it's the driver's bug. Land the real unbounded-access fix on your path anyway (doc-5 case 17) |
| `exit 139`, **deterministic**, gdb/coredump RIP lands in a GPU driver `.so` (`libvulkan_radeon.so`) at submit/present, faulting on a sentinel-`cmp` against a struct member | **Garbage Vulkan handle** we built — a malformed `Create*Info`, not a guest fault | Don't reverse the driver. Re-run under **`VK_LAYER_KHRONOS_validation`** (load it from the Steam runtime if the distro ships none) — it prints the **VUID + the named object/parameter** one message *before* the raw segfault, pointing straight at the bad `Create*Info` we assembled (e.g. a descriptor's `stage_flags`/layout not matching the SPIR-V stage that declares the binding) (doc-6 Entry 12, task-139) |
| `UnmappedMemory (read/write) of 0x0` | Guest null/uninit deref | Backtrace → what should have set it |
| `Exception vector N` / `int 0xNN` / printed `assertion … failed` | Deliberate guest abort | Backtrace to the caller; read the message; find the failed check (§5.4) |
| Handler returns OK but title rejects the result | Wrong data / wrong ABI | Log args; verify what the guest reads (§5.5) |
| Null/garbage deref whose backtrace bottoms at the ELF **entry / CRT** | Uninitialized process ABI frame | Disassemble `e_entry`: see how it reads argc/argv/env/auxv, then *construct that frame* — you own it, not the title (§5.6, doc-5 case 11) |
| `missing sceKernelLoadStartModule` / `sceKernelDlsym` (or the runtime dlopen's a `.prx`) | Dynamic module load | **Not a stub.** Load+link the `.prx`, run its `module_start` as a nested guest call, return a handle; `dlsym` = name→NID→resolve export (doc-5 case 12) |
| Runtime prints an errno-named fatal (`"Resource deadlock avoided"` / `EDEADLK`, an unexpected `EINVAL`/`EPERM`) that *your HLE returned* | Wrong HLE errno / ABI semantics | The runtime didn't fail — **you** handed it a spurious error. Log the exact call + the value you returned; check the POSIX/SCE contract (e.g. only ERRORCHECK mutexes return `EDEADLK`; NORMAL never does). Often nondeterministic if thread-timing-dependent (doc-5 case 13) |
| Deterministic garbage deref (`UnmappedMemory read of a tiny value` like `0x1d`) whose disasm masks a pointer (`p & ~0xfffff`, `& ~0xffff`) to reach metadata | HLE allocator ignored a guest **alignment** request | The guest allocator self-places metadata by masking an object pointer, so its region **must** be aligned as it asked. Find the alloc feeding that object (log size/alignment/returned-addr); if the returned base isn't aligned to the requested `alignment`, your `mmap`/direct-memory/flexible path is dropping the `alignment` arg. Honour it (doc-5 case 14) |
| `Exception vector 68` / `int 0x44` at an address with **no VMA name**, bytes `…CD 44…` | Guest called a retail-libc **unimplemented stub** (often `abort()`) | Not a lift/JIT bug — the guest hit its own fatal path. The retail libc compiles unprovided functions (and `abort`, `__cxa_pure_virtual`) as `int 0x44`; a symbol resolving to one links fine but traps when called. Peek the resolved import's first bytes in the linker to **name it**; then read the guest's own stdout just above the trap for *why* it aborted (doc-5 case 15) |
| Guest prints `mono_os_*wait: … failed with (110)` / an errno the runtime `g_error`s on; or a `-1` syscall the guest treats as an unexpected error | Wrong **FreeBSD** errno (or errno slot unset) | Every errno an HLE hands a FreeBSD-built runtime must be the FreeBSD value: **ETIMEDOUT 60** (not Linux 110), **EAGAIN 35** (not 11), **EDEADLK 11** (not 35). A `-1`/`-errno` return is worthless unless you also write the thread's errno slot (`ps4_cpu::set_errno`) — retail Sony libc / Mono read `*__error()`, not the return, and a missing write surfaces frames away as a `strlen(NULL)` in the runtime's error *formatter*. Doing the write at the `#[ps4_syscall(abi = posix)]` macro boundary makes it impossible to forget per-handler. Same family as case 13 (doc-5 case 15, 27) |
| Main thread **silently hangs** — no output, no fault, no busy-spin | HLE sync-primitive deadlock (often waiting on a **dead** worker) | gdb *attach* is usually blocked by Yama `ptrace_scope`; a fault-time breakpoint munmaps the arena. Read `/proc/<pid>/task/*/wchan` (no ptrace) — `futex_wait` = parked in an HLE Condvar/Mutex/Sem. Find which thread died and what it should have signalled (doc-5 case 15) |
| Real draws **record** + pipelines build, but the PNG is uniform (clear-color / black) — **zero fragments** survive any draw | Vertex/rasterization stage kills geometry (clipped positions, degenerate viewport/scissor, bad vertex/index fetch, NaN clip coord) | Don't reason about the whole pipeline — **bisect with isolation probes + PNG-check** each. A magenta `loadOp=CLEAR` proves present works AND that zero fragments overwrite it (upstream of fragment output). Then force one vertex/raster var benign (`gl_Position.w=1.0`, cull-none, full-viewport); if geometry appears, it's the vertex stage. Confirm with a data probe (dump fetched vertex floats + viewport + one draw's SPIR-V), not a guess (doc-5 case 20, doc-6 Entry 15) |
| Fragments **survive** + geometry rasterizes, but the PNG is **colorless** (uniform white / transparent / sparse-scrambled) despite draws issuing | Color pipeline, not the vertex stage — a texture that defers/detiles wrong, a UV/color varying reading zero, a dropped blend, or a scanout format/tiling mismatch | Extend case-20's bisection *into the fragment stage*: **force the PS output to each color source in turn** (constant red → interpolated vertex color → sampled texel → raw UV) and PNG-check — red-proves-present, vcol-proves-attr0, texel/UV-black bisects to a broken varying. Then chase the named source: a detiler for the bound texture's tile mode (a *diagonal* row-shear = padded pitch, not 2D swizzle), the per-stream vertex-`V#` params (one set may bind several streams), the derived `BlendKey` actually reaching the pipeline (not a hardcoded `blend_enable=0`), and the scanout attribute's *writer* (a garbage struct is often the writer's fault, not the reader's). PNG oracle is the only reliable color signal — logs over-claim (doc-5 cases 21/22, doc-6 Entries 16–19) |
| A PM4 trace shows **phantom packets** / a `TRUNCATED header=0xffffffff` **inside** the declared DCB size / a dropped frame tail — but **only on a *reused* command arena** (first use of the arena is clean) | Stale bytes in a hole your **HLE co-authored** into the guest cmdbuf — a builder wrote only its packet into a slot the guest reserved `numdwords` dwords for and left the tail untouched | "Clean first-use, corrupt on reuse" is the fingerprint of a stale-memory hole, not a decode logic bug (logic corrupts both passes). If your HLE writes a packet into a guest-sized slot (`sceGnmDraw*` / `sceGnmSet*Shader` write into the caller's cmdbuf), **fill the whole slot**: pad to exactly the runtime `reserved`/`numdwords` count with a trailing `IT_NOP` (`[packet][NOP]`; a header-only Type-2 NOP for a 1-dword gap) so the decoder walks past the hole. The reservation count IS the retail slot size — don't guess. Note a real corruption fix need not move the symptom you chased it for — confirm via the PNG oracle before crediting it (doc-5 case 23, task-166/167) |
| **Guest-emitted** per-frame state (texture/sampler binds, register writes) emits correctly for the first N frames then **drops from the DCB forever**, N == the double-buffer/context count; the resource stays resident (collapse is **emission-only**), and the drop coincides exactly with the first **REUSE** of a command context (NOT a stale-hole; the bytes are simply absent) | Our **synchronous** software GPU signals per-buffer completion **too early** — the guest middleware (Sony gnmx) CPU-polls a completion signal to recycle its command contexts and takes a fast "buffer already done → reuse without re-initializing" path, skipping the re-emit | Falsifiable experiment, not a guess: enumerate the completion signals the guest could poll between a submit and the next frame (equeue `sceKernelWaitEqueue`, EOP/EOS **memory-fence** label writes, flip labels). Env-gate an emulated completion LATENCY and **sweep it**, re-decoding per-frame emission counts. Deferring the wrong signal changes nothing; deferring the right one shifts the collapse by the latency; **withholding** the right one restores every-frame emission (confirm with the PNG oracle). Fix: surface completion through the primitive the guest actually **blocks** on (the equeue), not an inline memory-fence write it only uses as a recycle hint. No fixed finite latency sustains it at steady 60 Hz — the reused buffer's fence always reads "done" by the recycle check (doc-5 case 24, doc-6 Entry 20, task-157) |
| A title **deadlocks at boot** — no fault, no frames — and a stall trace (§3.10) shows a thread spinning in guest code right after `sceGnmSubmitCommandBuffers`, holding a lock the engine's submit-done thread waits on; and a completion signal you **withhold** for another title (case 24) un-wedges it when written (`UNEMUPS4_GPU_EOP_SYNC=1`) | **Two titles need OPPOSITE things from one GPU-completion signal** — one polls the EOP memory-fence label as its only completion (must be written), the other reads completion from the equeue and collapses if it is written | The discriminator is *which channel the guest listens on*, and it is **not** readable at the moment you must decide: both titles register an equeue event (`sceGnmAddEqEvent`), only one ever **waits** (`sceKernelWaitEqueue`). A wait-gated write is too late — the equeue title's first boot submits happen before its first wait, and 3 written boot frames re-trigger the case-24 collapse. **Default to the branch that cannot harm the working title (WITHHOLD), and switch away only on positive proof accrued over time** (no equeue completion ever collected AND past a short boot grace → the guest is a poller → write). A safety-critical per-frame default must be right from frame 0, not from the frame that finally proves identity (doc-5 case 31, doc-6 Entry 30, task-157 follow-up) |
| A guest draw **is submitted and is not deferred** (it appears in the capture and in the per-draw dump with a resolved pipeline), yet it **writes nothing** — the target keeps whatever it held | A draw-state register we never modelled, or a shader input that silently resolves to zero | Prove it reaches the GPU before theorising about it: diff the shader modules a capture hands to `vkCreateShaderModule` across a before/after run, and read the dumped `.spv` rather than the disassembly (ours has lied about operand signs and operand counts). Then enumerate the registers the guest **writes** for that draw and subtract the set our derivation **reads** — five such registers have turned up (`CB_TARGET_MASK`, `CB_SHADER_MASK`, `CB_COLOR_CONTROL`, `SPI_PS_INPUT_CNTL`, `VGT_PRIMITIVE_TYPE`) and two of them were the bug. `VGT_PRIMITIVE_TYPE` is the trap that hides best: a `DI_PT_RECTLIST` fill has three vertices that are **rectangle corners**, so read as a triangle it covers exactly half its target — and half a full-screen clear looks like no clear at all once a later pass overwrites the difference (doc-5 case 26, doc-6 Entries 25–26, task-184) |

| A query syscall is called **millions of times per second** with no file I/O, no frames and only crawling progress; sweeping the VALUE it returns changes nothing | **Unbounded enumeration** — the guest walks a table and discovers its end by being REFUSED, and we answer success for every index | Not a spin on the syscall: sample RIP (`UNEMUPS4_WATCHDOG`) and you land in real guest computation with no `call` in it. The loop is driven by the RETURN CODE, not by the data. Find the one call site (stub address from `hle_stubs.map` → the GOT slot holding it → the PLT → the `call`) and read the comparison right after it — the guest names its own terminating error. Then bound the table and return exactly that code past the end (doc-5 case 28, task-113.3) |
| A symbol is imported that the exporting module **provably exports** — it is loaded, its `.map` lists the name — yet it is stubbed missing, and the two modules import each other | **Dependency CYCLE in the module graph** | No walk order fixes this; post-order works on a DAG and a cycle has no topological order at all. Split loading into map-and-register-exports for the WHOLE graph, then relocate all of it. Watch for the two disguises: a needed MODULE name is not a LIBRARY name (`libSceLibcInternal` ships as `libc.prx`), and one file reached under two names will be mapped twice at two bases unless dedup is keyed by PATH (doc-5 case 29, task-29) |
| `[FATAL] missing symbol` whose NID has **no name** in `data/ps4_names.txt` | **Unnamed NID** — the hash is one-way and the name is simply absent | Do not brute-force it (~134k candidates cost an hour and found nothing). The import record also names the LIBRARY it comes from, which narrows a bare hash to one small API — make the linker print it. Then take the behaviour from the caller: find the call site as above, read the out-parameters it pre-zeroes, the value it range-checks, and any string it logs on the unexpected branch (a Fios2 warning named the whole function). Bind the handler to the raw NID (`#[ps4_syscall(nids = ["…"])]`); you never need the name (doc-5 case 30) |

The value of the taxonomy is that it tells you **which tool** from §3 to reach for, and
**whose bug** it is (yours: HLE/loader; the CPU engine's: lift; or the title genuinely
aborting because a precondition you control is wrong).

> **Keep this table current.** If you hit a wall that does not fit any row — a genuinely new
> *shape* of symptom — add a row here (and a worked case to doc-5) as part of that fix. A
> new *instance* of an existing shape does not need a row; a new shape does.

---

## 5. Attacking each class

### 5.1 Missing HLE symbol

The bread-and-butter wall. The title called a library function you have not implemented.

1. **Resolve the NID to a name**, then find the function's real signature and semantics
   (SDK headers, public ABI docs, the naming itself: `sceKernelMapNamedFlexibleMemory` is an
   mmap variant).
2. **Decide stub vs real** (§5.2). Implement the handler; register it under the right NID.
3. **Batch families.** If the title just hit `sceFooA`, it will hit `sceFooB/C/D` next.
   Stub the whole family in one pass (all the `scePthreadAttr*` getters/setters, the whole
   sysmodule/audiodec/net-init surface) to cut round-trips. Fill *out-parameters* with sane
   defaults, not just `return 0` — a getter that leaves its output untouched hands the
   caller garbage.
4. **Guard every guest pointer you dereference** (§5.3), *especially* on functions that also
   have a POSIX alias — see below.

### 5.2 Stub vs real implementation

- **Real** when the host has a 1:1 equivalent and the title depends on the behavior: file
  I/O → real host files; sockets → real BSD sockets; `mmap` of a file → actually read the
  file into the mapping; `fstat` → the real file size. If the title *reads back* what the
  call produced, it must be real.
- **Stub** when there is no host equivalent and the title only needs the call to *succeed*:
  a splash-screen hide, a music-player toggle, registering a callback you will never invoke.
  Return success and benign defaults so the title proceeds.
- **Report, don't lie, about caps.** A stub that returns a fake handle is fine; a stub that
  silently drops data the title will later read is a future ghost bug — anchor it (§2).

### 5.3 Host SIGSEGV — the identity-map pointer hazard

This class is specific to identity-mapped JITs and *will* bite you repeatedly. A guest
function with a POSIX alias (`pthread_mutex_init` aliasing `scePthreadMutexInit`) has *fewer*
arguments than the SCE form; the guest leaves **junk in the dropped argument register**
(seen: `0x44`, `0x11`, `0xffffffff…`). Your handler reads that "pointer", the JIT maps it
straight to a host address, and `strlen`/deref **segfaults the host** — no guest fault, no
backtrace, just `exit 139`.

- **Find it with gdb** (§3.4): the host backtrace names the handler and the bad value.
- **Fix systemically:** a single `is_guest_ptr(p)` predicate (is `p` inside the arena
  `[base, base+span)`?) used to guard *every* optional/out pointer before dereferencing.
  The arena is `NORESERVE`-backed so any in-range address is safe to read. Do not sprinkle
  ad-hoc `< 0x10000` checks; centralize.
- **Watch the bounds.** A lower-bound-only check (`>= 0x10000`) passes a huge junk value
  (`0xffffffff…`); you need both bounds.

### 5.4 Deliberate guest aborts (asserts, panics, `int 0x44`)

The title *chose* to abort because a precondition failed. This is good news: the message and
the failed check tell you exactly what precondition you got wrong.

1. **Get the backtrace** (§3.2) to find the caller of the abort helper.
2. **Read the message.** A runtime prints its own assertion text (`mono-threads.c:428,
   condition 'mono_thread_info_is_live'`) — that is a filename, a line, and a *named
   condition*. If the runtime is open source, that condition points you straight at what it
   expects. Recover *facts* (what field/state it checks) from the guest's own code and public
   sources — do **not** copy another emulator's implementation.
3. **Find the failing check.** Disassemble the caller: it typically calls a few predicates
   and aborts if one returns an error. Identify which predicate, and why *your* HLE made it
   fail. Example: a `__cxa_guard_release` abort was our `cond_broadcast` returning `EINVAL`
   for a statically-initialized condvar we had never seen — fix: treat an unknown condvar's
   signal/broadcast as a no-op success (doc-5 §3). Example: a Mono `staddr != NULL` assert was
   our `scePthreadAttrGetstack` returning zero bounds — fix: return the thread's real stack
   (doc-5 §5).

### 5.5 Wrong data / wrong ABI

The handler returned success, but the title rejects the result or crashes downstream. The
handler is *lying* — right shape, wrong content.

- **Log the arguments and the bytes involved.** The "invalid CIL image" wall was a valid
  file the title *read as zeros*, because `fstat` returned `size=0`, so the title `mmap`ed
  0 bytes. Only logging the `mmap`/`read` `len=0` exposed it (doc-5 §6).
- **Check struct offsets.** When you fill a guest struct (a `stat` buffer, an attr), the
  field offsets must match the guest's ABI exactly, or the title reads the size from where
  you wrote the mode. Verify against the ABI; a wrong offset is silent.
- **Check the whole pipeline.** open → fstat (size) → mmap (that many bytes, file-backed) →
  parse. A zero anywhere makes the parse fail with a message about the *parse*, not the
  zero. Walk it backward.

### 5.6 Uninitialized process ABI frame

The title's own CRT `_start` runs correct code and *still* faults on a null/garbage value —
because it read that value from a frame the **loader was supposed to build and didn't**. The
kernel-to-`_start` handoff (argc/argv/envp/auxv, or a platform-specific arg block) is *your*
responsibility, not the title's; skip it and the CRT propagates garbage into the runtime.

- **Tell it apart from a plain uninit deref (§the taxonomy):** the rbp backtrace *bottoms out
  at the ELF entry / CRT* (the lowest frames are at `e_entry` ± a few bytes), and the bad value
  traces back to a register or stack slot the entry set from the handoff frame — not to a
  global some library init forgot to set.
- **Disassemble `e_entry` first.** Extract the inner ELF (unwrap the container), then disassemble
  from `e_entry`. Read *how the entry consumes its input*: does it read argc from `(%rdi)` and
  argv from `%rdi+8` (a pointer-to-arg-block ABI), or pop them off `%rsp` (the SysV initial-stack
  ABI), or walk auxv? The first instructions tell you the exact frame to construct.
- **Build exactly that frame, minimally.** For the pointer-to-arg-block form: allocate near the
  top of the guest stack `[argc, argv[0], …, NULL, envp…, NULL, auxv AT_NULL]`, write the argv[0]
  string, and pass the block address in the register the entry reads. Put it *above* the scratch
  stack pointer so the CRT's own pushes never clobber it.
- **argv[0] is load-bearing.** A managed/CRT runtime derives paths from it (`dirname(argv[0])` →
  the app/assembly root). Point it at a real in-guest path under your mounts (e.g.
  `/app0/eboot.bin`), not an empty string — an empty argv[0] resurfaces later as a NULL-path
  assert deep in the runtime.

> **Run-environment aside (not a wall, but it will waste an afternoon):** keep the build and run
> toolchains **glibc-consistent**. If the binary links the host toolchain's glibc, run it against
> host libraries; do not run it under a dev-shell whose glibc is *older* (`version 'GLIBC_2.xx'
> not found`). Mixing nix-store libraries onto `LD_LIBRARY_PATH` can transitively pull the wrong
> glibc via their RPATH — resolve *all* runtime libs from one consistent set.

---

## 6. CPU lift gaps — delegate, don't inline

When the title hits `UnknownInstruction`, the wall belongs to the **CPU engine**, not your
HLE. A managed runtime (Mono/JIT'd .NET, MonoGame math) will exercise a long tail of
SSE/AVX instructions your engine may not lift yet.

- **The report gives you the bytes and mnemonic** ready to file.
- **Delegate the lift** to whoever owns the CPU engine (here: a separate focused agent
  working in the engine's repo, following *its* conventions and *its* differential-test
  harness). Do not hand-implement CPU semantics inline in the emulator — it belongs in the
  engine with the engine's tests.
- **Batch the family.** If the title hit `vhaddpd`, it will hit `vhaddps/vhsubpd/vhsubps/
  vaddsubps` too. Ask for the whole family per round-trip; each family shares a codepath.
- **Diagnose the real gap.** "Unimplemented" can mean: no decode, no IR op, or (subtly) a
  register form that lifts but a **memory-source operand** that does not. Verify which
  before assuming.
- **After it lands: bump the pin and *measure*.** Update your CPU-engine dependency pin,
  rebuild, and rerun **both** the title (did the opcode wall move?) **and** your regression
  examples (did the engine bump break anything, e.g. a SIMD example?). A dependency bump is
  not "done" until you have re-measured.

---

## 7. Where to start on day one

1. **Make the emulator *report* well** (§3.1) and **grow a guest backtrace** (§3.2). Without
   these you are blind; with them, every wall self-describes.
2. **Get the title to its entry point.** Unwrap the container, map the segments, resolve
   imports (unresolved → reporting stubs), run to the first fault. Expect it immediately.
3. **Run the loop** (§2). Read the report, name the wall's class (§4), apply the move (§5/§6),
   commit, rerun. Repeat.
4. **When you get an opaque host crash, switch lenses** (interp §3.3, gdb §3.4). When a
   handler "works" but the title rejects it, **log the args** (§3.9).
5. **Keep the map fresh.** Record each wall and fix (a task note, a running log). A
   bring-up is dozens to hundreds of small walls; the record is how you (and the next person)
   understand the shape of what the title needs.

The through-line: **turn every symptom into a precise cause before you touch code.** The
tools in §3 exist to do exactly that. Guessing is slow; a backtrace or a gdb stop or a
logged argument is fast. Reach for the instrument, read the fact, fix the smallest thing.

## Differential bisection + TOP-DOWN (added 2026-07-19, from the Celeste reveal-effect hunt)

When a retail title RUNS but renders/behaves WRONG (not a crash/wall — a correctness bug), the divergence sits in exactly one layer of the stack. Do not guess — **bisect the layers against a ground-truth oracle:**

```
managed game logic (C#)  →  CPU exec (Mono-AOT / x86jit)  →  HLE inputs (our syscall/HLE return values, FS, clock)  →  GNM command issue (guest PM4)  →  GPU translate (our recompiler + Vulkan)
```

Each layer has a ground-truth oracle; ask at each "is ours == real HW here?" and the first NO localizes the bug:
- **GPU translate:** RenderDoc capture of our frame; compare pass graph / bindings.
- **GNM issue:** the real-PS4 GNM scrape (`~/celeste-scrape-oracle`, task-168/172) — compare our DCB/VBUF vs real HW's for the same draw. If the guest ISSUES the same commands but the frame differs → our GPU is wrong; if it issues DIFFERENT commands → go up a layer.
- **CPU exec:** x86jit's **NativeOracle** (fork + real host CPU differential) — replay the guest's vector/scalar ops on a real CPU; zero divergence ⇒ not a lift bug ⇒ the INPUT is wrong.
- **HLE inputs / managed logic:** the decompiled game code (what it INTENDS) + which input it reads.

**The lesson that cost this session the most time: prefer TOP-DOWN for managed-runtime games.** Reverse-engineering upward from GPU pixels is slow and probe-heavy. When the title is MonoGame/FNA/Unity/Mono (managed + decompilable), READ THE GAME'S CODE FIRST:
1. Decompile to readable C# (**ILSpy / `ilspycmd`**, not raw `monodis` IL) — the game's own `.exe`/`.dll` are in the dump.
2. Find the exact effect/class that renders wrong; read what it computes and **which inputs it reads** (a time/delta, a progress `t`, a random seed, a texture dimension, an HLE value).
3. Check each of those inputs against what our emulator supplies. The game tells you what it needs — you just verify we feed it right.
4. Cross-reference the framework source (MonoGame/FNA are open on GitHub) for the exact semantics (e.g. SpriteBatch `sourceRectangle` → UV math).

Bottom-up (GPU→CPU) proves WHERE the divergence is NOT (invaluable for ruling out whole subsystems — we cleanly exonerated x86jit, GPU, RT, cache, FS). Top-down (game code → inputs) finds WHAT the divergence IS, fast. Use bottom-up to bound the layer, then top-down within it.

**Corollary — the maintainer's live eyes are the oracle for VISUAL state; headless agent conclusions that contradict them are wrong** (a classifier/scene-reachability artifact). Never report "fixed/gone/transient/playable" from logs or a headless run when the maintainer sees otherwise. See the `playable-needs-visual-oracle` lesson.
