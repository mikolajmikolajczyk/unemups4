---
id: doc-6
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

The companion doc-7 (*casebook*) walks concrete worked examples. Read this first for the
method, then doc-7 for the pattern-matching.

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
and read its logic. It cracked more than one wall in this project (see doc-7 §3).

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
  null-canary crash (doc-7 §4).

### 3.7 Logging syscall arguments

When a handler "succeeds" but the title still misbehaves, log the actual arguments the guest
passed. The `len=0` that revealed the `fstat`→`invalid CIL` chain (doc-7 §6) was invisible
until the mmap/read handlers logged their arguments. Add a temporary `info!` with the args,
rerun, read, remove it.

---

## 4. The taxonomy of walls

Every wall is one of a handful of shapes. Recognize the shape, apply the matching move.

| Symptom | Class | First move |
|---|---|---|
| `[FATAL] missing symbol: X [NID …]` | Missing HLE | Implement or stub X (§5.1) |
| `UnknownInstruction … : vfoo …` (bytes) | CPU lift gap | Delegate to the CPU engine (§6) |
| `exit 139` / core dump, no guest report | Host SIGSEGV | gdb → bad guest ptr in a handler (§5.3) |
| `UnmappedMemory (read/write) of 0x0` | Guest null/uninit deref | Backtrace → what should have set it |
| `Exception vector N` / `int 0xNN` / printed `assertion … failed` | Deliberate guest abort | Backtrace to the caller; read the message; find the failed check (§5.4) |
| Handler returns OK but title rejects the result | Wrong data / wrong ABI | Log args; verify what the guest reads (§5.5) |

The value of the taxonomy is that it tells you **which tool** from §3 to reach for, and
**whose bug** it is (yours: HLE/loader; the CPU engine's: lift; or the title genuinely
aborting because a precondition you control is wrong).

> **Keep this table current.** If you hit a wall that does not fit any row — a genuinely new
> *shape* of symptom — add a row here (and a worked case to doc-7) as part of that fix. A
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
   signal/broadcast as a no-op success (doc-7 §3). Example: a Mono `staddr != NULL` assert was
   our `scePthreadAttrGetstack` returning zero bounds — fix: return the thread's real stack
   (doc-7 §5).

### 5.5 Wrong data / wrong ABI

The handler returned success, but the title rejects the result or crashes downstream. The
handler is *lying* — right shape, wrong content.

- **Log the arguments and the bytes involved.** The "invalid CIL image" wall was a valid
  file the title *read as zeros*, because `fstat` returned `size=0`, so the title `mmap`ed
  0 bytes. Only logging the `mmap`/`read` `len=0` exposed it (doc-7 §6).
- **Check struct offsets.** When you fill a guest struct (a `stat` buffer, an attr), the
  field offsets must match the guest's ABI exactly, or the title reads the size from where
  you wrote the mode. Verify against the ABI; a wrong offset is silent.
- **Check the whole pipeline.** open → fstat (size) → mmap (that many bytes, file-backed) →
  parse. A zero anywhere makes the parse fail with a message about the *parse*, not the
  zero. Walk it backward.

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
   handler "works" but the title rejects it, **log the args** (§3.7).
5. **Keep the map fresh.** Record each wall and fix (a task note, a running log). A
   bring-up is dozens to hundreds of small walls; the record is how you (and the next person)
   understand the shape of what the title needs.

The through-line: **turn every symptom into a precise cause before you touch code.** The
tools in §3 exist to do exactly that. Guessing is slow; a backtrace or a gdb stop or a
logged argument is fast. Reach for the instrument, read the fact, fix the smallest thing.
