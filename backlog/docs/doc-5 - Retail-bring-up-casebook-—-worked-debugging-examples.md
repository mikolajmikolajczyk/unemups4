---
id: doc-5
title: Retail bring-up casebook — worked debugging examples
type: other
created_date: '2026-07-15 04:35'
---

# Retail bring-up casebook — worked debugging examples

Concrete case studies from a real bring-up (a Mono/MonoGame title, from "dies on the first
imported call" to "the managed runtime loads the game assembly"). Each case is one wall:
the **symptom** as it appeared, the **tool** that located it, the **reasoning**, the
**fix**, and the **lesson**. Read doc-4 first for the method and the toolbox; this is the
pattern library that makes the method concrete.

Each case is deliberately shown as *the reader would experience it* — the wrong first guess
is kept where it happened, because avoiding the wrong guess is the teachable part.

> **Maintaining this casebook (do this as you go).** This document is only useful if it keeps
> pace with the bring-up. When you clear a wall whose lesson **generalizes** — a new *shape*
> of bug, a new tool, a non-obvious cause — add a case here in the same
> symptom → tool → diagnosis → fix → lesson format, as part of that fix's commit. Keep the
> wrong first guess if there was one. Do **not** add a case for every routine missing-symbol
> stub; add one when a future reader would save real time from the story. If the wall is a
> new *class* (not just a new instance), also add a row to doc-4's taxonomy table (§4).

---

## Case 1 — "Where is module init?" (assumption vs. empirical recon)

**Symptom.** After all modules mapped and linked, the guest died `UnmappedMemory read 0x0`
deep inside libc. Cause: nothing had called the modules' initializers, so libc globals were
never set up.

**Wrong first guess.** "It's `DT_INIT`." Reading `DT_INIT` through the ELF library gave
`0x4000` for *every* module; the bytes there were `F4…` (`HLT`). So the conclusion became
"PS4 module_start must live somewhere exotic (the SCE module_info)". **This was wrong** and
cost time.

**Tool.** Stop theorizing; write a 30-line recon that dumps the *actual* program headers,
dynamic tags, and the bytes at candidate addresses for a real `.prx`.

**Diagnosis.** The recon showed `e_type=0xfe18` (a PRX), `e_entry=0`, raw `DT_INIT=0`, and
the bytes at module-relative vaddr 0 were `55 48 89 e5 41 57…` — a **real function
prologue** (`push rbp; mov rbp,rsp; …`), not `HLT`. The ELF library's computed
`DynamicInfo.init` had been misleading; the raw dynamic tag said 0, and offset 0 held the
`module_start`. So `module_start = module_base + e_entry` (== `+ DT_INIT`, both 0 here).

**Fix.** Call each dependency's `entry_point` (which the loader already computed as
`base + e_entry`) leaves-first before jumping to the executable entry. No exotic structure
needed.

**Lesson.** When a helper library's *interpretation* of a binary disagrees with what you
expect, dump the **raw** bytes yourself. An hour of wrong theory loses to ten minutes of
`xxd`. Verify against the artifact, not against a convenience API.

---

## Case 2 — libc raises an exception at init (a whole segment type was unmapped)

**Symptom.** During libc's `module_start`, the guest called `sceKernelDebugRaiseException` —
i.e. libc *deliberately* aborted very early.

**Tool.** Full program-header dump of the eboot (not just `PT_LOAD`).

**Diagnosis.** The `SceKernelProcParam` sub-structures (the libc heap params) were reached
through pointers into vaddr `0x2a00000`. That address fell in a gap between the two
`PT_LOAD` segments — **but** there was a `PT_SCE_RELRO` segment (`0x61000010`) covering
exactly `0x2a00000..`. The loader only mapped `PT_LOAD`, so RELRO — which holds *relocated,
then read-only* data — was never mapped. libc read its params from unmapped memory and
bailed.

**Fix.** Map `PT_SCE_RELRO` like a `PT_LOAD` (it is loaded, relocated, then set read-only).

**Lesson.** "Map the loadable segments" is not "map `PT_LOAD`". Retail SCE binaries put real
data in `PT_SCE_RELRO`. When a pointer dangles into an *unmapped gap*, enumerate **all**
program-header types, not the ones you already handle. (Masking the abort with a
`RaiseException` stub would have hidden a real loader gap — don't stub around a symptom you
haven't explained.)

---

## Case 3 — a deliberate `int 0x44` abort (backtrace + read the message)

**Symptom.** `Exception vector 68 (SIGILL) at 0x982e26`, repeated. Vector 68 = `0x44`; the
instruction was `int $0x44` — a guest abort trap. The RIP only named the *panic helper*.

**Tool.** The **guest backtrace** (doc-4 §3.2). This is the case that justified building it.

**Diagnosis.** The backtrace gave the caller chain into libc. Disassembling the caller frame
showed three predicate calls, each aborting with a distinct message on error; reading the
message strings out of the binary: `"__cxa_guard_release failed to acquire/release/broadcast
mutex"`. So libc's C++ static-initializer guard called pthread mutex-lock / unlock / **cond
broadcast**, and one returned an error. Our `cond_broadcast` returned `EINVAL` for a condvar
it had never seen — but that condvar was **statically initialized**
(`SCE_PTHREAD_COND_INITIALIZER`), so it was never explicitly created and never entered our
map.

**Fix.** `cond_signal`/`cond_broadcast` on an unknown condvar = a no-op **success** (a
broadcast with no registered waiters wakes nobody; that is not an error).

**Lesson.** A deliberate abort is a *gift*: the runtime tells you the failed condition. Get
the backtrace to the caller, read the panic string out of the binary, and identify which of
*your* return values violated the caller's expectation. The fix is usually "your HLE was too
strict about a statically-initialized primitive".

---

## Case 4 — a null "canary" (a *data* import that never resolved)

**Symptom.** `UnmappedMemory read 0x0` at a libc function whose first act was
`mov (%r14),%rax` with `r14=0`.

**Tool.** Disassemble around the fault; then **read the relocation** for the global that
`r14` was loaded from.

**Diagnosis.** `r14` was loaded from a RELRO slot; the relocation for that slot was
`Absolute64` against symbol NID `f7uOxY9mM1U` = **`__stack_chk_guard`** — the stack-smashing
canary, imported as **data**. Our linker wrote `0` for an *unresolved* `Absolute64` import,
so the slot held null, and libc dereferenced it.

**Fix.** Export `__stack_chk_guard` as an HLE **data** symbol: a small guest-resident word
(any consistent value) whose address resolves the relocation. This required a data-export
path in the HLE (previously only *code* stubs were exported).

**Lesson.** Not every import is a function. A **data** import that does not resolve is
silently written as null and blows up on first dereference — with no "missing symbol"
report, because the linker "handled" it. When a global is unexpectedly null, check whether a
relocation *should* have filled it and whether that relocation's symbol is an unresolved
import.

---

## Case 5 — Mono asserts `staddr != NULL` (give the runtime real facts)

**Symptom.** The runtime printed its own assertion: `mono-threads.c:386, condition 'staddr'
not met`, cascading into `mono_thread_info_is_live`. The runtime is now executing *its own
code* and checking an invariant.

**Tool.** Read the assertion (filename + line + named condition). `staddr` = stack address.

**Diagnosis.** The GC needs the current thread's **stack bounds** to scan the stack, and gets
them via `pthread_attr_getstack`. Our handler returned success but wrote **zero** for the
stack address → `staddr == 0` → assert.

**Fix.** Track each guest thread's real `(stack_base, stack_size)` (set when the thread
starts) and return them from `scePthreadAttrGetstack`.

**Lesson.** A getter that "returns 0 = OK" but leaves its **out-parameter** empty is a lie
the caller will catch. When a runtime asserts on a value, that value has a real source in
your emulator (here: the thread's actual stack) — wire the real thing, don't zero-fill.
(Recovering *what* the runtime needs from its own open-source assertion is fine; copying its
implementation is not.)

---

## Case 6 — a host SIGSEGV inside our own handler (gdb + `is_guest_ptr`)

**Symptom.** `exit 139` / core dump, **no** guest-fault report. The interpreter/JIT run loop
did not catch it, because it was not a guest fault.

**Tool.** Run the JIT build under **gdb**; let it catch the SIGSEGV.

**Diagnosis.** The host backtrace landed *inside our handler*:
`strlen(ptr=0x44) ← sce_pthread_mutex_init(…, name=0x44)`. The guest called the POSIX alias
`pthread_mutex_init` (2 args); the SCE handler read a 3rd "name" argument, but under the alias
that register held junk (`0x44`). Identity-mapped, `0x44` became a wild host address and
`strlen` walked off it.

**Fix.** A single `is_guest_ptr(p)` predicate (is `p` inside the arena?) used to guard the
optional `name` before dereferencing. Later hardened to check **both** bounds after a
different call left `0xffffffff…` in the register (a lower-bound-only check let it through).

**Lesson.** On an identity-mapped JIT, a bad *guest* pointer is a host crash, not a guest
fault — the run loop can't see it; only gdb can. Any HLE handler that dereferences an
optional or out pointer must range-check it first, especially functions with POSIX aliases
that drop arguments. Centralize the predicate; don't scatter magic numbers.

---

## Case 7 — "invalid CIL image" on a perfectly valid file (log the arguments)

**Symptom.** The runtime opened the core managed assembly (`mscorlib.dll`) from our
filesystem, then rejected it: `The file … is an invalid CIL image`.

**Wrong first guess.** "The file is bad / it's full-AOT and packed." Checked with `file` and
`xxd`: the file is a valid `PE32 … Mono/.Net assembly`, 3.4 MB, `MZ` header. So **our read of
it** was wrong, not the file.

**Tool.** Temporarily **log the arguments** of the file-mapping and read handlers.

**Diagnosis.** The logs showed `sceKernelMmap(…, len=0x0, …)` and `sceKernelRead(…, len=0x0)`
— the runtime asked to map/read **zero bytes**. Why zero? Because it first `fstat`'d the file
to get its size, and our `fstat` was a zero-fill stub → `st_size = 0` → map 0 bytes → parse
an empty buffer → "invalid CIL". A second, adjacent gap: even with a correct size, our `mmap`
ignored the `fd` and returned anonymous (zeroed) memory instead of the file contents.

**Fix.** (a) `fstat` returns the fd's real size; (b) file-backed `mmap` reads the file into
the mapping when `fd >= 0`.

**Lesson.** A parser complaining about *its input* often means the input was delivered wrong.
Walk the pipeline backward: parse ← mmap ← size-from-fstat. A single stubbed link (a
zero-size `fstat`) poisons everything downstream, and the error message names the *last*
step, not the poisoned one. The args in the log made the `len=0` obvious in seconds; without
them it was invisible.

---

## Case 8 — an argument that doesn't fit in a register (read the stack)

**Symptom.** `sceKernelMmap` has seven arguments; the syscall ABI passes six in registers.
The seventh (an out-pointer for the mapped address) was inaccessible, so the handler couldn't
return the address.

**Diagnosis.** SysV passes arguments 7+ on the stack. At the HLE stub's `SYSCALL` (which does
no `push`/`pop` before trapping), the callee frame is `[rsp] = return address`,
`[rsp+8] = arg7`, `[rsp+16] = arg8`, …

**Fix.** Expose the syscall-time guest `RSP` to handlers and add a `syscall_stack_arg(n)`
helper that reads `*(rsp + 8 + (n-6)*8)` (identity-mapped guest memory). The 7-arg handler
takes its 6 register args normally and reads the 7th from the stack.

**Lesson.** The register ABI has a fixed width; real functions exceed it. Know exactly where
the extra arguments live (the callee stack frame, offset by the return address) and provide a
seam to reach them, rather than giving up on the syscall.

---

## Case 9 — the assemblies exist but the runtime looks elsewhere (mount aliases)

**Symptom.** The runtime searched `/app0/mono/4.5/mscorlib.dll` and `lib/mono/4.5/…`; the
dump shipped `mscorlib.dll` at the package **root**. Path mismatch → not found.

**Tool.** `find` the dump for the file; compare to the paths the runtime opens (visible
because `open` logs its path).

**Fix.** Alias the framework search directories onto the dump directory (`/app0/mono/4.5` and
`/app0/lib/mono/4.5` → the dump root), and make relative paths (`lib/mono/4.5/…`) resolve
through the same union so they hit the same place. A longer mount prefix wins, so the aliases
take precedence for those paths.

**Lesson.** "File not found" during asset loading is usually a **layout** mismatch, not a
missing file. Diff *where the file is* against *where the runtime looks* (its `open` calls
tell you the latter), and bridge the two with mounts rather than moving the game's files.

---

## Case 10 — an unimplemented CPU instruction (delegate + measure)

**Symptom.** `UnknownInstruction … vroundsd $0x9,%xmm1,%xmm0,%xmm1` with the raw bytes. Then,
after that landed, a *steady stream* of sibling AVX ops (`vpunpckldq`, `vhaddpd`,
`vmovntdq`, `vphaddd`, …) as the managed runtime's math ran.

**Diagnosis.** These belong to the **CPU engine**, not the HLE. A managed/JIT runtime
exercises a long tail of SSE/AVX. Some gaps were "no op at all"; others were subtler — a
register form that lifted but a **memory-source operand** that did not.

**Fix / workflow.** Delegate each to a focused agent working in the CPU engine's repo,
following its conventions and its **differential test harness** (validate against a hardware
oracle). Batch families per round-trip. After each lands: bump the engine dependency pin,
rebuild, and **re-measure both** — did the title's opcode wall move, *and* do the regression
examples (including a SIMD-heavy one) still pass?

**Lesson.** Keep CPU semantics in the CPU engine with the CPU engine's tests; don't inline
them in the emulator. Expect managed runtimes to demand a whole family of vector ops — batch
them. And treat every dependency bump as unverified until you have re-run both the new target
and your old regressions.

---

## Case 11 — a null deref inside the title's own CRT (the arg frame we never built)

**Symptom.** Deep in Mono's app-domain setup, two printed asserts — `assertion 'filename !=
NULL' failed` (eglib `g_path_get_dirname`, `gmisc`) — then `UnmappedMemory (write) of 0x0` at
`mov %rbx,(%rax)`, `rax=0`. The guest backtrace *bottomed out at the ELF entry* (`hlt_gadget`
→ two frames a few bytes past `e_entry` → the runtime).

**Diagnosis.** The faulting store writes into `*(global)` where the global was a NULL array
pointer. Disassembling `e_entry` (extract the inner ELF, `objdump -b binary --adjust-vma`)
showed the CRT's contract: `mov (%rdi),%r14d` (argc = `*rdi`) and `lea 0x8(%rdi),%r15` (argv =
`rdi+8`) — a **pointer-to-arg-block** ABI. We were calling the entry with `rdi = start_rsp` and
an *unconstructed* stack, so `argc` was garbage. Mono sized its saved-argv copy as
`g_malloc((argc+1)*8)`, which failed for the junk count → the global stayed NULL → the store
faulted; and `dirname(argv[0]=NULL)` produced the `filename != NULL` asserts on the way there.

**Fix.** In the main-thread launch (thread setup), build the arg block near the top of the
guest stack — `[argc=1, argv[0]=&"/app0/eboot.bin", NULL, envp NULL, auxv AT_NULL]`, write the
path string, pass the block's address in `rdi`. `argv[0]` is a real `/app0` path so Mono's
`dirname(argv[0])` resolves the assembly root to the dump mount. Placed *above* the scratch
`start_rsp`, so the CRT's own pushes never touch it.

**Lesson.** When the backtrace bottoms at `e_entry`, suspect the **process ABI frame you own**,
not a title bug. Disassemble the entry to learn the exact contract (arg-block ptr vs. SysV
stack vs. auxv) and build precisely that. `argv[0]` is load-bearing for runtimes that derive
paths from it. (doc-4 §5.6.)

---

## Case 12 — the runtime dlopen's a native module (load, don't stub)

**Symptom.** After the managed runtime was up: `missing symbol sceKernelLoadStartModule`, and
right behind it `sceKernelDlsym`.

**Diagnosis.** This *looks* like a plain missing-HLE wall (§5.1), and the reflex — stub it,
return success — is **wrong here**. A temporary logging stub (return a fake handle, log the
path argument) revealed the real request: `sceKernelLoadStartModule('/app0/scePlayStation4.prx',
…, pRes=<guest ptr>)` — Mono's `Sce.PlayStation4.dll` P/Invoke is *dynamically loading the
native interop `.prx`* and then `dlsym`-ing individual functions by name. A fake handle would
have made every later `dlsym` return garbage. The `.prx` is a real file at the dump root, not
in `DT_NEEDED` (genuinely runtime-loaded).

**Fix.** Implement it for real by *reusing the loader you already have*: translate the guest
path via the mount table, load+link the `.prx` and its deps leaves-first (the same module-tree
routine boot uses), then run its `module_start` as a **nested guest call** (the mechanism that
already runs TLS destructors / `pthread_once`), write the result to `*pRes`, and return a real
module handle (with a path→handle map for idempotency). `dlsym` = hash the requested name to
its NID and resolve it against that module's exports; a genuine miss (Mono probes optional C++
symbol variants) correctly returns `-ENOENT` and the runtime continues.

**Lesson.** "Missing symbol" is a *class*, not a verdict — a loader/dlopen call wants the real
behavior, not a stub. The temp logging stub that reads the argument is what tells the two
apart. And a dynamic loader is mostly plumbing you already own (container unwrap → parse → link
→ nested `module_start`); wire the existing pieces rather than writing a second loader.

---

## Case 13 — a NORMAL mutex handed a spurious `EDEADLK` (model the mutex *type*, not a bool)

**Symptom.** Nondeterministic, thread-race teardown crashes after the runtime was well up
(platform + audio + savedata + graphics init, real shaders bound, a draw recorded). It varied
run-to-run: sometimes `UnmappedMemory (read) of 0x1d` on the main thread, sometimes
`Exception vector 68 (SIGILL)` inside a TLS destructor during teardown — but always with
`mono_os_mutex_lock: pthread_mutex_lock failed with "Resource deadlock avoided" (11)` printed by
the guest runtime.

**Diagnosis.** "Resource deadlock avoided" is the strerror for `EDEADLK` (11). Our HLE
`mutex_lock` returned `EDEADLK` whenever a non-recursive mutex was re-locked by its current
owner. Targeted logging (tid, owner, locks, type, and every lock/unlock/trylock on the one
faulting handle) showed the culprit precisely: the guest fast-path-locks its mutex via
`pthread_mutex_trylock` (uncontended acquire, no blocking call) and unlocks via
`pthread_mutex_unlock` — a clean `trylock`→`unlock` pairing we saw hundreds of times — and then
called the *blocking* `pthread_mutex_lock` on a handle it already held (`owner==tid`,
`locks=1`). We returned `EDEADLK`. Init logging showed the handle was created with a **null
attr** — i.e. a `PTHREAD_MUTEX_NORMAL` (default) mutex, which is exactly what Mono's
`mono_os_mutex` uses. POSIX: only `PTHREAD_MUTEX_ERRORCHECK` returns `EDEADLK` on self-relock;
a NORMAL mutex's self-relock is *undefined* (a real deadlock) and **never** a checked error.
Mono treats any `EDEADLK` from lock as fatal. Root cause: we collapsed all three mutex types to
one `is_recursive: bool`, so every non-recursive mutex behaved as ERRORCHECK.

The `0x1d` fault was a *separate*, pre-existing wall (a Mono function byte-scanning a
near-null pointer read from `*(struct+0x10)`); the `EDEADLK`/SIGILL was teardown fallout that
fired *after* it. Fixing the mutex bug removed the nondeterministic teardown noise and made the
remaining wall deterministic — a prerequisite for attacking it cleanly.

**Fix.** Model the three POSIX/Orbis mutex types (`Normal`/`ErrorCheck`/`Recursive`) instead of
a bool; read the real type from the `pthread_mutex_init` attr (FreeBSD/Orbis libthr:
`ERRORCHECK=1`, `RECURSIVE=2`, else NORMAL; null attr = NORMAL). Self-relock: only `ErrorCheck`
returns `EDEADLK`; `Recursive` counts up; `Normal` **also** counts up (a benign recursive
acquire — real POSIX would deadlock, never error, and the guest never intends a genuine
self-deadlock here). `trylock` of an already-owned handle: only `Recursive` re-acquires; others
report `EBUSY`. (`crates/kernel/src/sync.rs`, `crates/libs/src/libkernel/pthread.rs`,
`crates/core/src/kernel.rs`.)

**Lesson.** A spurious `EDEADLK` from a NORMAL mutex is a classic HLE trap: the POSIX mutex
*type* is not cosmetic — it changes the self-relock contract, and only ERRORCHECK checks for
deadlock. Collapsing the types to "recursive or not" silently makes every default mutex an
ERRORCHECK one, and any runtime that self-relocks a default mutex on a fast path (or that treats
`EDEADLK` as fatal) will crash *nondeterministically* because it depends on thread timing. When
you see `EDEADLK`/"deadlock avoided" from a managed runtime, check whether you honor the mutex
type from the init attr before assuming the guest really deadlocked.

---

## Case 14 — the `0x1d` fault was an alignment request we ignored (honor the guest's alignment)

**Symptom.** Deterministic (once Case 13's mutex noise was gone) `UnmappedMemory (read) of 0x1d`
at `rip 0x1a8d620` (eboot.bin+0x105620), instruction `cmpb $0x0,(%rdi,%rdx)`, where `rdi`
came from `*(rax+0x10)` and held `0x1d` — a tiny integer, not a pointer.

**Diagnosis.** Disassembling the enclosing routine showed a memory allocator that derives a
*section header* from a chunk pointer by masking: `section = chunk & ~0xfffff` (clear the low
20 bits = round down to a **1 MB** boundary), then reads `section->free_chunk_map` at
`section+0x10` and byte-scans it. The assertion strings just past the fault named the mechanism
exactly: `mono/sgen/sgen-los.c`, `section->free_chunk_map[i]`, line 189 — Mono **SGen's Large
Object Space**. LOS sections are 1 MB-aligned *by contract*, and the whole
`chunk & ~0xfffff` trick only recovers the section base if each section actually starts on a
1 MB boundary.

Throwaway logging on every alloc path (`find_free_region`: size+returned addr;
`sceKernelAllocateDirectMemory`: its `alignment` arg + returned addr; `map_flexible`:
size+addr) pinned it in one line. The last alloc before the fault was
`sceKernelAllocateDirectMemory length=0x100000 alignment=0x100000` (1 MB, **1 MB alignment
requested**) → returned `0x4cee1c000`, whose `& 0xfffff` is `0x1c000` — **not** 1 MB-aligned.
SGen then computed `section = 0x4cee00000`, read a `free_chunk_map` pointer out of an unrelated
neighbour, got `0x1d`, and byte-scanned it into the null page.

Root cause: our HLE dropped the guest's alignment. `sceKernelAllocateDirectMemory`/
`sceKernelMapDirectMemory` took `_alignment` and ignored it; `VmMemoryManager::find_free_region`
aligned only to 16 KB and bumped `heap_cursor` by the raw size, so after any
non-1MB-multiple allocation the cursor drifts off the 1 MB grid.

**Fix.** Thread the alignment through and honor it. Added `map_aligned` /
`find_free_region_aligned` to `VirtualMemoryManager` (defaults ignore alignment → delegate,
so the many test stubs are untouched), overridden in `VmMemoryManager` to round the
"allocate-anywhere" base up to `max(requested_align, 0x4000)`; added `mmap_aligned` to
`KernelInterface`/`Process` (default → plain `mmap`); and passed the `alignment` arg from
`sceKernelAllocateDirectMemory` through it. `sceKernelMapDirectMemory` needs nothing — in this
HLE it only echoes the address AllocateDirectMemory already placed aligned. (`crates/core/src/memory.rs`,
`crates/core/src/kernel.rs`, `crates/memory/src/vm_backend.rs`, `crates/kernel/src/process.rs`,
`crates/kernel/src/bridge.rs`, `crates/libs/src/libkernel/mman.rs`.) The `0x1d` fault
vanished on both interp and jit; boot advanced through TLS/mutex init and real GNM draws
(`sceGnmSetVsShader`/`SetPsShader350`/`DrawIndexAuto count=3`) to a new wall: missing
`sceKernelGetdents` (directory enumeration of `/app0/Content/Tutorials`).

**Lesson.** When a guest allocator recovers metadata by **masking an object pointer**
(`p & ~0xfffff`, `& ~0xffff`, …), the region it lives in *must* be aligned exactly as the
allocator asked — the alignment argument to `mmap`/direct-memory/flexible-memory is not a hint,
it is load-bearing for the allocator's own pointer math. An HLE that under-scopes an
"allocate anywhere" placement to a coarse fixed granularity and silently drops the caller's
`alignment` will hand back a base that's aligned enough to *touch* but not enough for the
guest's masking to land — producing a deterministic garbage deref (a tiny value like `0x1d`)
one metadata hop later, far from the alloc site. The tell is a fault address that is a small
integer read from `*(struct+off)` right after a masking instruction; trace it back to the alloc
feeding that struct and compare the returned base against the requested alignment.

---

## Case 15 — a bare `Exception vector 68` was the guest calling `abort()` off a wrong errno (the errno-ABI chain, and how to name an `int 0x44` stub)

**Symptom.** After the first `sceGnmDrawIndexAuto`, two guest threads hard-fault with
`Exception vector 68 (SIGILL) at 0x982e26`, faulting instruction `mov $0xcccccccc,%edx`.
No name, no VMA annotation (0x982e26 is below every module base), no printed assertion.
Both threads reach it as an indirect-call target from the same caller. It reproduces under
**both** backends (a 150 s interp run that "passed" had merely *timed out before reaching
the fault* — never conclude "interp is clean" from a run that stopped short of the fault
point; measure to completion).

**The chain that cracked it (four tools, each naming the next):**
1. **Module-base log** placed 0x982e20 inside the loaded `libc` (`Linker: Loading 'libc'
   at 0x92c000`). So the target is a real libc export, not random corruption.
2. **gdb, early-break-on-dispatch.** A breakpoint at the *fault* is useless — the faulting
   thread's teardown `munmap`s the arena before you can read it. Instead break somewhere
   hit **early and often** with the arena alive and the image loaded — `break
   app/unemups4/src/main.rs:18` (the syscall dispatcher). The guest is identity-mapped, so
   `x/24i <guest_addr>` disassembles the loaded image directly. This showed the caller does
   `call <PLT>` and the PLT is `jmp *<GOT>(%rip)`; the GOT slot held 0x982e20, whose bytes
   are `push %rbp; mov %rsp,%rbp; int $0x44; ...` — and **`int $0x44` = vector 0x44 = 68**.
3. **Linker peek names it.** `int 0x44` is the Orbis "unimplemented function" trap: retail
   libc compiles functions it does not provide (and `abort`, `__cxa_pure_virtual`) as that
   stub. A symbol resolving to one links *successfully* (never hits the missing-symbol
   lazy-stub path) but traps the instant it's called. A three-line peek in the linker's
   JumpSlot/GlobDat arm — read the resolved target's first 6 bytes, warn if `[4]==0xCD &&
   [5]==0x44` — turned the mystery into `Import resolved to an Orbis unimplemented-stub (int
   0x44) … : abort [NID L1SBTkC+Cvw]`. **The guest was calling `abort()`** — its own fatal
   path, not a JIT/HLE/lift bug.
4. **Guest stdout names *why* it aborted.** `sys_write` to fd 1/2 goes to the host stdout,
   so the runtime's own message is right there before the trap: `mono_os_cond_timedwait:
   pthread_cond_timedwait failed with … (110)`. Mono `g_error`s when a timedwait returns
   anything but `0` or `ETIMEDOUT` — and **ETIMEDOUT is 60 on FreeBSD/PS4, not the Linux
   110 our HLE returned**. Same class as Case 13's `EDEADLK`, a different constant.

**Fix + the second errno.** `sync.rs` timedwait paths now return 60. That let the main
thread past the abort — into a *silent hang*. A worker had died earlier on
`mono_os_sem_timedwait: sem_trywait failed with "No error" (0)`: our `sem_trywait` returned
`-1` on an unavailable permit but never set `errno`, so Mono read a stale `0` and aborted
the worker; the main thread then blocked forever waiting on a semaphore the dead worker
would have posted. `sem_trywait` must set `errno = EAGAIN` (FreeBSD **35**) — added a
`ps4_cpu::set_errno` that writes the thread's errno slot. Worker survives → signals main →
Celeste enters its render loop.

**Debugging the hang without ptrace.** gdb *attach* is blocked by Yama `ptrace_scope`, and
the fault-time breakpoint munmaps the arena. `/proc/<pid>/task/*/wchan` needs no ptrace and
named it instantly: the guest-main host thread sat in `futex_wait` (an HLE Condvar/Mutex),
not a busy-spin or a vsync poll — i.e. a sync-primitive deadlock, pointing straight at the
dead worker.

**Lessons.** (a) A bare `int 0x44` / "Exception vector 68" with no VMA is the guest calling
a retail-libc unimplemented stub — most often `abort()`; peek the resolved import bytes in
the linker to name it. (b) When the guest calls `abort`, the *reason* is usually one line of
guest stdout above the trap — the runtime prints its own errno-named fatal. (c) Every errno
an HLE hands a FreeBSD-built runtime must be the **FreeBSD** value (ETIMEDOUT 60, EAGAIN 35,
EDEADLK 11 — all differ from Linux), and `-1` returns are worthless without also setting the
errno slot. (d) For a silent hang, `/proc/*/task/*/wchan` beats a ptrace-blocked gdb.

---

## Case 16 — a `NeedsGcn` defer that was really a parser reject (dump the bytes; a defer category can name the wrong layer)

**Symptom.** With draws finally resolving, every one deferred at
`ShaderPairResolution::NeedsGcn` — *"bound to a non-recompilable (.sb GCN) shader — deferring
draw"* — and the frame presented clean white (own `UNEMUPS4_DUMP_PNG` read, [png oracle]). The
obvious read: the GCN→SPIR-V recompiler can't compile the title's shaders yet, so the next work
is recompiler instruction coverage. **That read was wrong.** `NeedsGcn` is returned whenever the
provider chain hands back `Err`, and the recompiler is only *one* source of that `Err` — the
`.sb` **parser** (`parse_sb`) is another, upstream of it. The defer category named the outcome,
not the failing layer.

**The tool that named the real cause.** Rather than start editing the recompiler, dump the actual
shader bytecode: a new env-gated diagnostic `UNEMUPS4_DUMP_GCN=<dir>` writes the register-derived
shader window (`SPI_SHADER_PGM_LO/HI` → address) to a file on each resolve, *before* parse — the
point is to capture shaders that **fail to parse**. One lean run, then `RUST_LOG=warn` showed the
truth in the logs, uniform across every draw: `GCN shader parse rejected … m_length does not match
header offset`. Not a recompile defer at all. The recompiler had never run.

**The mechanism (RE'd from 22 dumped shaders).** Our validator required the `OrbShdr` footer to sit
*tight* against the code: `code_start + m_length == header_addr`. Real Orbis shaders don't — the
footer sits at `code_start + m_length + gap`, where `gap` (8..64 bytes observed) is an
input-usage/hash table between the code and the footer. The code itself ends in `s_endpgm`
(`0xBF810000`); every shader is 256-byte-aligned. An offline harness (dump → extract per-shader
code bins → `decode_all`+`recompile`, a sub-second loop vs a 2-minute guest run) confirmed the
layout on all 22 and then mapped the *actual* recompiler walls behind the parser (VOP2 int/pack,
SMRD scalar loads, exec-mask mov, the `s_swappc` fetch-shader call).

**Fix.** Relax the validator to `code_end <= header_addr <= code_end + MAX_FOOTER_GAP` and set the
code range to exactly the `m_length` bytes. A relaxation this loose re-admits false-positive
`OrbShdr` magic inside the code, so guard it with the format's own discriminator — the last code
dword must be `s_endpgm` (a stray magic's coincidental `m_length` won't land on a terminator). A
later code-review caught that the guard broke a **ps4-memory** test that also feeds `parse_sb`:
tightening a widely-consumed parser needs `cargo test --workspace`, not just the owning crate.

**Lessons.** (a) A defer/error *category* (`NeedsGcn`) can point at the wrong layer — it names the
last observer, not the failing one. Confirm which layer actually failed before committing to a
plan built on the category's name. (b) The confirming tool is the same one as every other case:
dump the raw bytes (`UNEMUPS4_DUMP_GCN`) and read them, don't theorize about the recompiler from a
white frame. (c) A dump hook that captures input to a *rejecting* parser must run **before** the
parse. (d) An offline decode+recompile harness over dumped bytecode turns a 2-minute-per-iteration
guest loop into a sub-second one — build it the moment you're iterating on shader internals.

---

## The through-line

Across all twelve cases the move is identical: **turn the symptom into a precise cause with a
tool before touching code.** A raw-bytes dump beat a wrong theory (Case 1); a full
program-header enumeration found an unmapped segment type (Case 2); a guest backtrace named a
deliberate abort's caller (Case 3); a relocation read explained a null (Case 4); an assertion
string named a missing fact (Case 5); gdb caught a crash the run loop couldn't (Case 6);
logged arguments exposed a `len=0` (Case 7); an entry-point disassembly exposed an arg frame we
never built (Case 11); a throwaway logging stub told a real dlopen apart from a stubbable symbol
(Case 12); per-handle lock/unlock/type logging exposed a spurious `EDEADLK` from a NORMAL mutex
(Case 13); a one-line-per-alloc log caught an ignored 1 MB alignment request that corrupted
SGen's section math (Case 14); a chain of module-map → early-break disasm → linker byte-peek →
guest stdout turned a nameless `int 0x44` SIGILL into a named `abort()` off a wrong `ETIMEDOUT`,
and `/proc/*/task/*/wchan` named a silent deadlock a ptrace-blocked gdb couldn't reach (Case 15);
a raw bytecode dump (`UNEMUPS4_DUMP_GCN`) exposed a `NeedsGcn` "recompiler" defer as an upstream
`.sb` **parser** reject — the defer category named the wrong layer (Case 16).
The tools in doc-4 §3 exist to produce exactly these facts. Guessing is slow and
often wrong; a backtrace, a gdb stop, a logged argument, or an `xxd` is fast and certain. Reach
for the instrument first.

## Case 17 — an "intermittent executor SIGSEGV" that was a driver (RADV/ACO) crash, not our seam (spirv-val + swap the backend to place the blame)

**Symptom.** Celeste boots fully and submits real ~4 MB command buffers; the executor reaches draws
and builds pipelines, but ~half the headless runs SIGSEGV (exit 139) with **no** clean guest-fault /
UnknownInstruction / missing-symbol diagnostic — a bare exit 139 right after the first
`unhandled PM4 opcode 0x13 (IT_INDEX_BUFFER_SIZE)`. Runs under `RUST_LOG=…=debug` **survived**. The
standing hypothesis (recorded in the task notes) was ours: a bad/racing register-derived V#
dereferenced through the *unbounded* `IdentityMem` on the new CB/vertex resolve path, or shared
provider state racing across submit threads. "Intermittent + masked by logging = a race in our code"
is the natural read — and it was wrong.

**The three tools that named the real cause, in order.**
1. **`gdb -batch` with `run; bt; thread apply all bt`, looped until it faults.** Caught the SIGSEGV
   first try. The backtrace put the fault **inside `libvulkan_radeon.so`**, called from
   `ash … create_graphics_pipelines` ← `create_host_pipeline` ← `run_command_list` **on the display
   thread** — i.e. inside `vkCreateGraphicsPipelines`, not on the submit thread and nowhere near a
   descriptor decode. A raw-`??` frame in a system `.so` is itself the signal: *the crash is in the
   driver, on input we handed it.*
2. **Dump the exact input and validate it.** An env-gated hook wrote the VS/PS SPIR-V modules being
   fed to pipeline creation; `spirv-val` accepted **both**. So the module is not malformed by the
   spec — the blame is either a driver bug or an invalid *pipeline state* combination.
3. **Swap the driver's shader backend to bisect driver-vs-us.** `RADV_DEBUG=llvm` (LLVM backend
   instead of the default **ACO**) — the crash **vanished** across every run that reached the
   executor. `spirv-val` clean **and** RADV-LLVM clean, only RADV-ACO crashes ⇒ the module is valid
   and the fault is an **ACO compiler bug**, heap/ASLR-sensitive (hence "intermittent", hence "gone
   under logging" — the timing/allocation pattern shifts).

**Why the "race in our code" hypothesis was structurally impossible (proven by audit, not by the
absence of a repro).** (a) The bounded seam is already watertight on the exec draw path: every
register-derived descriptor decode routes through `bounded_read()` or reads the V# straight from
SGPRs (no memory read), and every guest-*byte* upload routes through `BoundedMem::read_bytes_ranged`
— a bad V# is a clean `Err` → whole-draw defer, never an unbounded host deref. (b) No cross-thread
race touches it: all submit threads serialize through the single `static DRIVER: Mutex<GnmDriver>`
(lock held across `Executor::run`), and the submit→display `RunCommandList(cmds, signal)` hand-off is
**synchronous** (submit blocks on `signal.recv()`), with the SPIR-V crossing as an immutable
`Arc<[u32]>`.

**Lesson (generalizes).** When a retail wall is a *bare* SIGSEGV with no emulator-level diagnostic
and it's "intermittent / masked by logging", do **not** assume the race is yours. Get one `gdb` `bt`
first: if the top frames are in a system `.so`, the crash is in that library on your input. Then
prove your input is well-formed with the domain validator (`spirv-val` here) and **swap the
library's internal backend** (`RADV_DEBUG=llvm`, or a different ICD) to split "our bug" from "their
bug" — a swap that makes a *valid* input stop crashing convicts the driver. Only after that spend
effort on your own code. The concrete residue worth landing anyway: audit + close the *actual*
unbounded guest accesses on the path (here the EOP/EOS `write_label` store, now VMA-validated through
the bounded seam before the identity write) — strictly safer, even though it was not the crash.

## Case 18 — a managed-runtime `abort()` was a spurious HLE errno (EBUSY) from an internal manager, not the libs shim (hexdump the trap site, trace the errno to its origin)

**Symptom.** Celeste (Mono full-AOT) ran, then a thread took an `int 0x44` trap ("Exception vector 68 at
0x982e26") that killed the process before any geometry. Two red herrings: (a) it looked like a CPU lift
gap (a nearby thread genuinely had one — the libfmod VMASKMOVPS, a *separate* fault, Case-worthy on its
own); (b) the trap address sat in guest code with no obvious cause.

**The hunt — hexdump the trap site, then read the log one line up.** The new fault reporter
(`crates/cpu/src/exec.rs::guest_hexdump`, added the same session) printed the bytes at the faulting RIP:
`ba cc cc cc cc  b9 0b 00 02 a0  90  0f 0b` = `mov edx,0xcccccccc; mov ecx,0xa002000b; nop; ud2` — i.e.
the emulator's OWN Orbis "unimplemented/abort" stub (`int 0x44` lifted to `Exit::Exception{vector:68}`),
not a guest instruction. So "vector 68" was a *deliberate* trap, and the real question was *who called
abort()*. One line up in the log: `mono_os_mutex_init: pthread_mutex_init failed "Device busy" (16)` —
Mono aborts when its mutex init fails. EBUSY (16) was the true cause.

**Mechanism.** The errno did NOT come from the libs `pthread.rs` shim (which returns 22/12/… never 16).
It came from `SyncManager::mutex_init` (`crates/kernel/src/sync.rs`): `if map.contains_key(addr) { return
EBUSY }`. But `mutex_lock` **lazily inserts** a placeholder entry for any never-init'd handle, so
map-presence ≠ a live/held mutex. A guest that locked-before-init, or legitimately re-init'd a mutex on a
seen address (Mono inits many early), tripped `contains_key == true` → spurious EBUSY → fatal abort.

**Consequence (fix, task-145).** On a known address, replace the entry with a fresh `HostMutex` and
return 0 — real pthread/libthr semantics for re-initializing an *unlocked* mutex. The Mono abort vanished
and Celeste ran 30 s+ into gameplay **asset streaming** (level `.bin`s, character portrait atlases) and 35
GPU submit/present/draw events. **Generalizable lesson (doc-4 taxonomy row): a fatal guest `abort()`/trap
is often your OWN unimplemented/abort stub firing — hexdump the trap site to confirm it's a stub, then
read the log one line up for the errno/reason that triggered it, and trace that errno to its true origin
(here an over-strict internal manager, NOT the syscall shim that merely forwarded it). errno FIDELITY in
HLE matters: a managed runtime treats a wrong errno as fatal.** Distinct thread's VMASKMOVPS was a real
x86jit gap (filed in the x86jit backlog), unrelated to this abort.

## Case 19 — one fatal trap can be TWO independent faults on two threads; hexdump the RIP to tell your own stub from a real gap

**Symptom.** Celeste (Mono full-AOT, pervasively AVX/SSE) died with a guest `SIGILL` reported as "Exception
vector 68 at `0x982e26`" while a nearby thread also looked wrong. The tempting read is one wall; the reality
was two, on two different threads, of two different classes.

**The hunt — classify each fault by hexdumping its own RIP.** "Vector 68" is `int 0x44` — but that alone
doesn't say whose. The fault reporter now hexdumps the guest bytes at the faulting RIP
(`crates/cpu/src/exec.rs::guest_hexdump`, wired into `report_unknown_instruction` + `report_exception`,
task-144), so read those bytes and classify:

- **Thread A (`0x982e26`):** the bytes are our OWN Orbis abort stub — `int 0x44` lifted to
  `Exit::Exception{vector:68}`, ending in `ud2` (`0f 0b`). So this "SIGILL" is a *deliberate* trap we fired,
  not an unlifted instruction. The real cause is one log line up (an upstream errno → Mono `abort()`; the
  EBUSY-mutex chain of Case 18).
- **Thread B (libfmod, a *different* RIP):** the bytes are `c4 e2 3d 2e 11` = `VMASKMOVPS ymm[rcx],ymm8,ymm2`
  — a genuine x86jit lift gap (VEX.256 masked AVX *store*, opcode `0x2E`, a NEW family past the earlier VEX
  work, distinct from the existing EVEX k-mask VMaskMov). That one is a real CPU-engine gap, filed in the
  x86jit backlog (never edited directly).

**Mechanism / lesson.** Two faults in one run are not automatically the same wall. The three-way
classification from the RIP bytes: (1) our own stub (an `int 0xNN` / `ud2` shape) → the fault is deliberate,
hunt *up* the log for the trigger; (2) ordinary guest code → the guest hit its own fatal path; (3) a real
unlifted instruction (a decodable-but-unhandled opcode) → a CPU-engine gap to delegate. Do the hexdump
*first* and split the run into its independent faults before assuming a single root cause. Here the abort-stub
thread was tracked to an over-strict HLE errno (Case 18 / task-145) and the libfmod thread to a distinct
x86jit gap — two fixes, not one.

## Case 20 — "geometry submits but the frame is black": bisect present-vs-fragment-vs-vertex with isolation probes

**Symptom.** Celeste records a *real* render workload — ~800–1366 GNM draws, ~157–254 `SubmitAndFlip`, 986
binds per run, with real register-derived VS/PS and real vertex counts — yet the presented frame is uniform
BLACK, then (after the texture-defer fix) exactly the clear color. Real draws, zero visible pixels. Guessing
among "present is broken / fragments are killed / vertices are clipped / the wrong image is blitted" is a trap;
each is a different subsystem.

**The hunt — force one variable benign at a time and PNG-check.** The frame is the only ground truth (logs
lie — see the PNG-oracle rule), so bisect the pipeline with temporary probes, each of which *proves* one stage
in or out:

1. **Present/blit vs everything upstream — the clear-color probe.** Force a magenta `loadOp=CLEAR` color. The
   whole PNG turned uniform magenta → the swapchain, render pass, and present blit all work end-to-end, AND
   **zero fragments** overwrite the clear anywhere (no draw produces output). That single probe splits the
   problem: it is *upstream of fragment output*, not in present.
2. **Fragment-survival vs vertex stage — force the geometry visible.** With present ruled in, force benign
   vertex/raster state one at a time: force `gl_Position.w = 1.0` in the VS (also candidates: force-cull-none,
   force-full-viewport). Forcing `w=1.0` made geometry *appear* → the vertex stage was the culprit: positions
   were leaving clip space. That pointed straight at the `dst_sel`/`gl_Position.w = NaN` root cause (doc-6
   Entry 15b: the position `V#`'s `w` channel is `SQ_SEL_1` = const `1.0`, but the fetch read the raw padding
   dword → NaN → every primitive clipped).
3. **Then confirm with a data probe, not a guess.** A runtime probe dumping the fetched vertex floats +
   viewport/scissor + MVP, plus a disasm of one recorded draw's SPIR-V, confirmed `dst_sel=[4,5,6,1]` and
   `gl_Position.w = row3.(v4..v7)` — the exact mechanism, not an inference.

**Lesson.** When draws record but no pixels appear, do NOT reason about the whole pipeline at once. Force one
stage's output benign (clear-color for present; `w=1.0`/cull-none/full-viewport for the vertex stage) and
PNG-check — each probe collapses the search space to one subsystem. The magenta clear proved present + proved
zero fragments in one shot; forcing `w=1.0` isolated the vertex stage; the data probe named the bug. This
bisection took Celeste's black frame straight to its first rasterized pixels (task-149 → 152).

---

## Case 21 — "fragments survive but the frame is colorless": force the PS output down the color-source ladder (red → vertex color → texel → UV)

**Symptom.** After the black-frame fixes (case 20), Celeste's geometry rasterizes and fills the screen — but
every fragment is uniform WHITE. Fragments now *survive* (unlike case 20), yet no color, texture, or alpha
reaches them. The failure is somewhere in the color pipeline: the sampled texel, the interpolated vertex
color, the UV that indexes the texture, or the blend that composites them. Guessing among "the texture is
white / the UV is wrong / the sample isn't wired to the output / blend eats it" is again a trap — each is a
different stage.

**The hunt — force the PS output to each color source in turn and PNG-check.** Case 20's ladder proved
present + vertex position; extend the same idea *inside* the fragment shader. Behind an env gate, override the
recompiled PS output, one source at a time, and PNG-Read each frame:

1. **Constant red.** Override the PS output to a constant red. The PNG turned fullscreen red → raster, present,
   and geometry coverage all work; the fragment shader *runs* and its output *reaches the screen*. The failure
   is in *what the PS computes*, not whether it draws.
2. **Interpolated vertex color (attr0).** Override the output to the interpolated vertex color. Yellow → attr0
   fetches and interpolates correctly. The color stream is fine.
3. **The sampled texel, then the UV (attr2).** Override to the sampled texel → BLACK. Override to the raw
   interpolated UV → also BLACK. Two black probes bisect the failure to **attr2 (UV) reading exactly ZERO** —
   the sampler is fine, it is being asked to sample at (0,0).

That pointed straight at the multi-stream vertex fetch bug (doc-6 Entry 17): the atlas VS recovers three
interleaved vertex-buffer `V#` (pos/color/UV) but the recompiler bound one SSBO and packed only stream 0's
`num_records`, so attr2 clamped to index 0 → zeros. Fixing the fetch made the forced-UV probe show a clean
gradient — the confirming data signal, not an inference.

**Lesson.** The black-frame bisection (case 20) has a color-frame sibling: when fragments *survive* but the
frame is colorless, **force the PS output to each color source in turn** — a constant (proves the PS runs and
presents), the interpolated vertex color (proves attr0), the sampled texel, the raw UV (proves attr2/the
sampler input) — and PNG-check each. Red-proved-present, vcol-proved-attr0, texel+UV-both-black-bisected-to-
attr2 collapsed a four-stage color pipeline to one broken varying in three probes. This is the fragment-stage
analogue of the magenta-clear / force-`w=1.0` isolation probes: force one variable benign (or one source
constant) and let the PNG oracle name the surviving stage.

---

## Case 22 — a garbage struct is often the *writer's* fault, not the reader's — and every color-pipeline assumption this session was overturned by a probe

**Symptom.** Two recurring traps surfaced across the Celeste color bring-up, both worth naming.

*(a) The reader looked guilty but was innocent.* The videoout display buffer came out DEGENERATE — a 1×3
surface (doc-6 Entry 18) that presented white-on-black. The obvious suspect was `read_videoout_attr`: it
*read* the 1×3, so surely its offsets were wrong. They were **correct all along** (`width@+12`, `height@+16`,
matching the vendored SDK header). The struct it read was garbage because `sceVideoOutSetBufferAttribute` — the
7-arg **writer** that should have *filled* that struct — had been stubbed as a 3-arg no-op. The reader
faithfully reported the garbage the missing writer left behind.

*(b) Every first guess was wrong, and only a probe corrected it.* Across the five color-pipeline fixes this
session, the *initial* mechanism hypothesis was wrong every time, and each was overturned by a direct probe:
- "macro bank/pipe swizzle" → **linear-aligned padded pitch** (overturned by detiling raw bytes offline until
  glyphs straightened — doc-6 Entry 16).
- "the const buffer / sampler collides on one binding" → **three interleaved vertex streams, only stream 0's
  params packed** (overturned by the forced-UV probe of case 21 — doc-6 Entry 17).
- "render-to-texture with a GPU resolve→sample" → **double-buffered direct scanout, no resolve packet exists**
  (overturned by the PM4 trace — doc-6 Entry 18).
- "a tiling / pixel-format problem" for the colorless splash → **the pipeline hardcoded `blend_enable=0`**
  (overturned by dumping `texture_image` mid-run: RGB=0, alpha=218 → draws cover the screen but overwrite,
  which is a blend fact, not a format fact — doc-6 Entry 19).

**Lesson.** Two rules, both hard-won:
1. **When a struct reads as garbage, audit the writer before you blame the reader.** A reader that produces a
   degenerate value is often faithfully reporting what a missing/no-op *writer* left in memory — check the
   producer's signature and that it actually stores, not just the consumer's offsets.
2. **In the color pipeline, the PNG oracle (the orchestrator Reads the actual pixels) is the ONLY reliable
   signal — logs and agent self-reports over-claimed throughout.** Every mechanism assumption here was wrong on
   the first pass and only a direct probe (offline detile, PM4 trace, forced-output PNG, a mid-run channel
   dump) corrected it. Do not accept a mechanism story until a probe *shows* it; treat "logs say it worked" as
   a hypothesis, not a result.

## Case 23 — a phantom PM4 packet was STALE BYTES in a hole our HLE left in the guest cmdbuf: when you co-author a reserved slot, fill the whole slot

**Symptom.** Celeste's splash logo presented as a solid **white bar** (untextured white-dummy), and
a PM4 trace of a *reused* command arena (frame 3+, once the DCB ring wraps) showed the decode walk
going haywire only on reused arenas: an orphan `SET_SH_REG` appearing *right after* a `DRAW_INDEX`,
phantom ring-buffer writes, and a `TRUNCATED header=0xffffffff` **inside** the declared DCB size that
halted the walk before `EVENT_WRITE_EOP` — so the frame tail (final draw + EOP) was silently dropped.
The first arena's use was clean; only the *second* use of the same arena corrupted.

**The hunt — "first use clean, reuse corrupt" points at uninitialized-vs-stale memory, not logic.**
Our HLE co-authors the guest DCB: the gnmx `sceGnmDrawIndex*` / `sceGnmSet*Shader` builders are, on
real hardware, guest-side functions that write a fixed PM4 packet into a cmdbuf slot the guest
**reserved `numdwords` dwords for** (it advances its cursor by `numdwords`). Our emitters wrote only
`pm4.len()` dwords and left the remaining `reserved - pm4.len()` tail **untouched**. On a first-use
(zeroed) arena that hole decodes as harmless zero packets. On a **reused** arena the hole still holds
the *previous frame's* bytes — and our decode walk, which cannot tell an intended packet from a
leftover, reads those stale bytes as real packets. The `0xffffffff` was simply a stale dword; the
orphan `SET_SH_REG` was last frame's real register write surfacing through this frame's hole. The
"first use clean / reuse corrupt" asymmetry is the tell: a logic bug corrupts both, a stale-memory
bug corrupts only what was written before. The fix already existed one module over — `emit_shader_set`
appends a trailing `IT_NOP` sized to cover its own body so a decoder walks past the whole block; the
draw path just never applied it, and even the shader path only self-sized to its *documented* length,
not to the (larger) runtime reservation. Pad every reserved slot to exactly `numdwords`:
`[real packet][IT_NOP covering the rest]` (a header-only Type-2 filler for a 1-dword gap). The runtime
`reserved` arg **is** the authoritative retail slot size — you never have to guess the packet length.

After padding, the reused-arena DCB decoded clean end-to-end: every draw followed by its pad NOP,
`TRUNCATED header=0xffffffff` gone (18→0 across the run), `EVENT_WRITE_EOP` reached in all flips
(2/37 → 40/40), and the previously-dropped tail draws now execute. **But the logo stayed white.** The
leading hypothesis had been that the phantom `SET_SH_REG 0x2c0c` clobbered PS user-data before the
logo draw → degenerate texture → white-dummy. Removing the phantom disproved it: the corruption was
real and worth fixing (it dropped whole frame tails), but it was **not** the cause of the white logo —
that is a separate atlas-T#-provenance wall, now exposed on an un-corrupted stream.

**Lesson.** Three rules:
1. **If you co-author a guest buffer into a slot the guest sized, fill the WHOLE slot, not just your
   packet.** A partial write over a reused region leaves a hole that reads as stale prior-frame data.
   The runtime reservation count is the slot size — pad to it with a trailing NOP; don't guess lengths.
2. **"Clean on first use, corrupt on reuse" is the fingerprint of a stale-memory / uninitialized-hole
   bug**, not a logic error — logic corrupts both passes equally. Let that asymmetry point you at *what
   was written last time*, not at your decode.
3. **Fixing a real corruption need not fix the symptom you chased it for.** Confirm the mechanism you
   *predicted* (here: PS user-data clobber → white logo) actually moves the pixels via the PNG oracle
   before crediting the fix; when it doesn't, say so and hand the clean stream to the next wall rather
   than forcing a positive.

## Case 24 — a SYNCHRONOUS software GPU reported completion "too early", so the guest's gnmx recycled its command buffers WITHOUT re-emitting per-frame texture binds — the logo rendered as a white bar (task-157)

**Symptom.** Celeste's steady-state splash renders the logo as a solid WHITE BAR: the three content
draws re-emit their 8-dword atlas T# (`SET_SH_REG 0x2c0c`) + S# (`0x2c14`) only on frames 0 and 1,
then drop them forever (`3,3,0,0,0…`). Real-PS4 ground truth (`data/celeste-real-dcb/`) re-emits all
three EVERY frame. The atlas descriptors stay resident on frame 2 (the collapse is EMISSION-ONLY — the
guest simply stops putting the binds in the DCB), and the drop coincides EXACTLY with the first reuse
of the double-buffered command context. Four prior phases exonerated the constant engine, DMA, arena
staleness, register persistence, value-dedup, GC/data-loss, and the virtual clock (falsified twice,
byte-identical). Decompiling MonoGame's `TextureCollection` showed a pure managed `_dirty` bitmask that
`Present` resets all-dirty every frame — so the gate had to live in the NATIVE gnmx layer (stripped
AOT), invisible to static analysis.

**Move — turn the correlation into a falsifiable experiment.** "Collapse on reuse" pointed at a
completion signal the reuse path polls. Enumerate them: the guest registers ONE equeue event
(`sceGnmAddEqEvent` type=64 EOP) and blocks on `sceKernelWaitEqueue` every frame, AND every flip DCB
carries one `IT_EVENT_WRITE_EOP` that writes a **memory fence** (ping-ponging between two per-context
addresses). Our executor is synchronous, so both fire before the guest resumes. Env-gate an emulated
completion LATENCY and sweep it, re-decoding per-frame bind counts:

- Deferring the **equeue** signal — even forcing the wait to observe "GPU not done" — changed NOTHING.
- Deferring the **EOP memory fence** shifted the collapse out by exactly the latency (`collapse frame
  = depth + 2`).
- **Withholding** the fence entirely made gnmx re-emit all three binds EVERY frame (`3,3,3,…`) — with
  no deadlock — and the PNG oracle showed the fully TEXTURED "Matt Makes Games Inc. / presents" splash
  (gradient, font atlas, bokeh particles) instead of the white bar.

**Mechanism.** gnmx recycles its double-buffered command contexts by CPU-polling the EOP memory fence
("is the GPU done with this buffer, may I refill it WITHOUT re-initializing it?"). Instant completion
always answers "already done" → fast recycle path that SKIPS re-emitting the buffer's per-draw state,
including the atlas binds. Real hardware writes that fence asynchronously (the GPU runs ~1–2 frames
behind), so gnmx sees the reused buffer as still in flight and re-records the FULL state every frame.
Reporting completion **too early** is as wrong as reporting it late.

**Fix.** Split emit from store in `crates/gnm/src/exec.rs`: `emit_label` DEFAULTS to pipelined
completion — it does not surface the EOP/EOS memory fence synchronously. The guest's real frame-sync
GPU-completion signal is the equeue event (still fired on submit-done); the raw memory label is only
gnmx's recycle hint. `UNEMUPS4_GPU_EOP_SYNC=1` restores the old inline write for A/B. Verified
bidirectionally (default → textured; sync → white bar).

**Lessons.**
1. **A synchronous software GPU can mislead guest middleware that polls a memory fence to time buffer
   recycling.** When guest EMISSION (not just your decode) collapses on command-buffer REUSE, and the
   resource is still resident, suspect the completion signal the reuse path polls — not your renderer.
2. **Sweep the latency, don't reason about it.** Deferring the *wrong* signal is a clean negative that
   rules it out; deferring the *right* one moves the symptom monotonically. The equeue-vs-memory-fence
   split fell straight out of the sweep.
3. **No fixed finite latency reproduced real HW here** — at steady 60 Hz the reused buffer's fence is
   always old enough to read "done" by the recycle check, so latency only shifts the collapse. Prefer
   surfacing completion through the primitive the guest BLOCKS on (the equeue), not an inline
   memory-fence write it uses only as an optimization hint.

## Case 25 — a plausible-but-wrong SDK status code silently gated an entire game-state transition; the "error" log we dismissed as normal WAS the bug (task-170)

**Symptom.** Celeste reached the 2D "CELESTE" attract screen, rendered + animated every frame, but
never advanced to the interactive menu and never polled the pad: `scePadReadState ×0` across a full
run, despite `scePadOpen` succeeding. A wall of `ERROR: Couldn't get event from User Service:
0x80960009` scrolled every frame — dismissed across multiple sessions as "benign NO_EVENT steady-state
polling."

**The four-pass hunt (each pass falsified the previous lever, which is the point).** (1) A per-thread
execution trace proved nothing was hung — the Mono-AOT tick loop ran, draws advanced ~220/s; the gate
was a game-state condition, not a stall. (2) IL decompile (monodis) of Celeste + MonoGame proved the
managed input path is unconditional — no managed gate. (3) A module-attributed **syscall caller probe**
(read `[rsp]` at each `Exit::Syscall` = the caller's return address, bucket by owning module VMA)
proved `scePlayStation4.prx` itself is loaded, executing, and the live caller of every working draw +
`scePadOpen` + `sceUserServiceGetEvent` — refuting a tempting "the prx never started / it's a loader
gap" reframe. (4) That left the gate *inside* the prx's own `GamePad::GetState`. The prx is a SELF but
**fake-signed with a cleartext ELF payload** (magic `4F153D1D`, `\x7fELF` at file offset 0x160), so its
x86-64 code is directly disassemblable: `System::Update` polls the pad **only** on the branch
`sceUserServiceGetEvent(&ev) == 0x8096_0007`; every other return prints the "Couldn't get event" error
and skips the poll. Our HLE returned `0x8096_0009` for the empty queue.

**Root cause.** `0x8096_0009` is `SCE_USER_SERVICE_ERROR_NOT_LOGGED_IN`; the empty-queue code is
`SCE_USER_SERVICE_ERROR_NO_EVENT = 0x8096_0007` (one hex digit). The wrong code sent the prx down the
error branch every frame → pad never polled → `GetState` returned disconnected forever → stuck on
attract. Fixing the constant makes `scePadReadState` fire (0→nonzero) and the title advance; live, the
controller navigates. It then exposed the next walls: missing `sceImeUpdate` / `sceMouseRead` /
`scePadSetVibration` (the last INPUT-TRIGGERED, so only reachable by actually playing).

**Lessons.**
1. **A benign-looking recurring log line is a prime suspect, not background noise.** We had the exact
   wrong value (`0x80960009`) printed on screen for multiple sessions and read past it. When a title
   won't leave a state, grep every per-frame log line the guest ITSELF emits and check each value
   against the SDK — the guest is often telling you which branch it took.
2. **A wrong-but-plausible return code is worse than a missing symbol.** A missing symbol aborts loudly;
   a wrong status silently routes the guest down a dead branch with no crash. Verify HLE return
   *values* against `data/oo_sdk/include/.../errors.h`, not just that a handler exists.
3. **Fake-signed retail SELFs are readable.** When a `.prx`/`.self` wraps a cleartext ELF (ELF magic a
   few hundred bytes into the file), you can statically disassemble the guest's own native logic and
   read the exact branch — no emulator-side guessing. This is how the `== 0x8096_0007` test was found.
4. **Caller-module attribution (`[rsp]` at the syscall boundary) tells you WHO is calling.** It refuted
   two whole reframes (managed-gate; prx-never-started) by proving the prx was the live caller of the
   very calls that worked. Cheap, decisive, reusable.

## Case 26 — five independent defects between the guest's intent and the screen, and why each fix "did nothing" until the last one landed (task-179/180/184)

**Symptom.** Celeste's main menu rendered the menu text over a flat gradient; the 3D mountain behind it was
missing. RenderDoc showed the mountain drawn correctly in an early colour pass and lost thereafter.

**What it actually took.** Five separate bugs, each necessary, none sufficient:

1. `SPI_PS_INPUT_CNTL_n.OFFSET` routes pixel-shader attribute slot *n* to a vertex-shader export parameter.
   We assumed identity. Celeste's blur reads its UV from `attr0` while programming `OFFSET = 1`, so it got
   the vertex colour — a constant — and sampled one texel.
2. An offscreen render target's sampled extent is the CONTENT extent, not the alignment-padded
   `CB_COLOR0_PITCH`. Sampling `[0,1]` over the padded surface read ~6% of never-written padding per axis,
   per hop, compounding along the two-hop bloom chain.
3. A vertex shader that reads `v0` **directly** — no fetch shader — never had `gl_VertexIndex` seeded; only
   the fetch path did. All three vertices of every guest full-screen fill collapsed to one point.
4. Even after (3), the index resolved on the **first** read only: ALU emitters untrack their destination
   before evaluating sources, so an in-place update (`v_and_b32 v0, -2, v0`) read zero.
5. `VGT_PRIMITIVE_TYPE` was never modelled. The fills are `DI_PT_RECTLIST`, whose three vertices are
   rectangle corners; read as a triangle they cover half the target.

**The trap, and it is the point of this case.** Every one of (1)–(4) was correct and verified, and after
each the picture was unchanged — because a later defect still swallowed the result. Twice this nearly caused
correct work to be doubted or reverted. **"The picture did not change" is evidence about the last defect in
the chain, not about the fix you just landed.** Establish separately that a fix reached the hardware: after
(3) we diffed the shader modules two RenderDoc captures handed to `vkCreateShaderModule` — 1524→1576 bytes —
which proved the new module was executing while the screen still looked identical. Without that, (3) looks
like a failed hypothesis.

**Three measurement traps, all of which produced confident wrong answers:**

- **RGB cannot tell you whether a target's clear landed.** Under premultiplied-over into a never-zeroed
  target the fixed point is `dst = f·blur/f = blur` — a *correct-looking* blur with no saturation. Only the
  ALPHA channel distinguishes "cleared each frame" from "accumulating since boot". Two independent analyses
  nearly concluded the opposite from RGB.
- **The disassembler lied twice**: it dropped VOP3 `neg`/`abs` modifiers, making a symmetric blur kernel read
  as one-sided and a scale-down as a scale-up, and it printed a spurious third source on two-source forms.
  Read the dumped `.spv` — the module actually handed to Vulkan is the only artefact that cannot be wrong
  about what ran.
- **A readback we had been using as a probe was itself broken**, reporting a bright target as near-black. It
  had been packing rows at the content width while every reader indexed at the padded stride. A diagnostic
  built on a semantics path inherits that path's hardest problem for no benefit; give diagnostics their own
  route.

**What actually broke each deadlock** was never more instrumentation. It was env-gated knobs the maintainer
could flip and judge by eye, one targeted RenderDoc comparison of a draw's input against its output, and —
once the GPU state snapshot existed — reading a dumped `.spv` and fitting candidate blur kernels to dumped
render-target PNGs to refute a descriptor hypothesis numerically (RMSE 1.62 versus 3.76). Build the tool
that answers the class of question, then ask it; the snapshot tool paid for itself the day it landed.

## Case 27 — a syscall's failure was signalled entirely through the errno TLS slot, not its return value — and a confident root-cause theory was wrong until a cheap contingency gate caught it (task-191)

**Symptom.** Pressing CLIMB to enter gameplay crashed the game thread with `UnmappedMemory (read) of 0x0` at
`libc!strlen`, up a call chain `vasprintf → vsnprintf → _Mbtowc → strlen(NULL)` — a `%s` fed a null pointer
inside the retail libc's error formatter. A fresh save has no `/savedata0/0.celeste`, so a `stat` on it
returns ENOENT; the crash was in the guest's *reaction* to that ENOENT.

**The wrong turn, and why it was seductive.** A prior session had disassembled the faulting frames from a
module dump and built a clean, plausible theory: retail Mono calls `sceKernelStat`, reads the return as an
**SCE error code**, and does `errno = sce_to_errno(ret)`; our handler returned a raw `-2`, which is not a
valid SCE code, so Mono fell through to a `"%s: unknown error"` formatter whose code→string lookup returned
NULL. The fix implied was a systematic ABI cleanup: make the `sce*` family return SCE codes
(`0x8002_0000 | errno`), split from the POSIX aliases (`stat`/`_stat`) which the OpenOrbis example ELFs call
with sign semantics. That cleanup was built (a `ps4-core::errno::Errno` converter, an `#[ps4_syscall(abi =
sce | posix)]` macro projecting the return convention, the fs family split into shared-impl + two adapters)
— all correct hygiene. **It did not fix the crash.**

**What caught the wrong premise.** Because the theory hinged on *which* import the crash path used
(`sceKernelStat` and `stat` are distinct NIDs but were collapsed onto one stub), the plan deliberately shipped
a throwaway confirmation: a one-shot `warn!` in each of the two stat adapters naming which id actually fired.
The maintainer's run printed the answer, and the per-thread HLE breadcrumb ring nailed it exactly:

```
#812 stat(...) -> 0xfffffffffffffffe     // HLE name "stat" (the POSIX import), ret = -2
#813 _Errno(...) -> 0x40f234200          // caller then fetches the errno pointer
```

The crash path is the **POSIX `stat`**, not `sceKernelStat` — and the disassembled frame reads
`call __error; cmpl $0x2,(%rax); jne <crash>`. **The caller ignores the stat return value entirely.** It
calls `__error()`/`_Errno` to get the guest's per-thread errno slot, reads `*errno`, and its graceful
"file-not-found → empty slot" branch needs `*errno == 2`. Our POSIX `stat` returned `-2` but **never wrote the
errno slot**, so the slot held stale bytes (≠ 2), the graceful branch was skipped, and the `"unknown error"`
formatter ran with a NULL `%s`. The entire SCE-return-code theory was about a value the caller never reads.

**The fix — one place, systemic.** The contract already existed and was even documented: `ps4_cpu::set_errno`
writes the same per-thread slot that `current_errno_addr()`/`__error` hands back, and its doc comment says a
POSIX-failing handler *must* call it. The fs handlers never had. Rather than sprinkle `set_errno` through a
dozen handlers, the `#[ps4_syscall(abi = posix)]` macro's error arm now emits it for **every** POSIX handler:
`Err(e) => { ps4_cpu::set_errno(e.0); Errno::to_posix(e) as u64 }`. The return stays `-errno` (the OpenOrbis
libc negates the return into errno itself; retail Sony/Mono reads the TLS via `__error`) — both caller
families satisfied, and all six example baselines stayed byte-identical because the extra TLS write is inert
for a caller that reads the return. Celeste reached the New Game screen.

**The lessons.**
- **A syscall's failure can live entirely in the errno TLS slot.** A `-errno` (or even `-1`) return is
  invisible to a caller that inspects `*__error()` — and retail Sony libc / Mono's OS-primitive wrappers do
  exactly that. Any POSIX HLE handler that can fail must call `ps4_cpu::set_errno`; doing it at the macro
  boundary makes "return path" and "errno path" impossible to forget. (This is the doc-4 taxonomy row "errno
  slot unset", seen before in case 15 for `mono_os_*wait`; here it wore the disguise of a `strlen(NULL)` crash
  in an error *formatter*, three frames removed from the syscall.)
- **When a fix's premise is uncertain, ship the confirmation with it.** The SCE-code theory was detailed,
  disassembly-backed, and wrong about the one fact that mattered — which import the crash path used. A
  two-line throwaway log settled in one run what an hour of static reasoning had gotten backwards. Build the
  cheap empirical gate into the change; do not trust a root cause you cannot observe firing.
- **The breadcrumb ring is the fastest disambiguator you have.** It named the call (`stat`, not
  `sceKernelStat`), its return (`-2`), and the very next call (`_Errno`) — the whole mechanism in three lines,
  no disassembly required to see that the return value was being ignored.

## Case 28 — a title ran 3.5 million syscalls a second and made no progress: it was enumerating a table and we never told it where the end was (task-113.3)

**Symptom.** The second title (UE4) stopped dying on missing symbols and started running — forever, without
arriving anywhere. `scePlayGoGetLocus` was called **67 million times per 20 seconds**. Over the same window
`sceKernelOpen` stayed at 4, `gpu present` stayed at 2 frames, and memory allocation crawled forward at ten
reservations per 20 s. Neither stuck nor progressing.

**Three readings that were wrong, and what killed each.**

*"It is a deadlock."* Three worker threads sat in `futex_wait`. But the in-flight report (§3.7) named what
they were in — `scePthreadCondWait`, all three, blocked since startup — and the completed-syscall table
contained **no `scePthreadCondSignal` at all**. Nothing ever signals them: an idle worker pool, correctly
parked. Not the problem. One dump ruled this out.

*"The emulator is re-entering the syscall."* A plausible emulator-side bug: if the return path did not advance
RIP, one guest call would re-execute forever. `UNEMUPS4_WATCHDOG` sampling killed it — the RIPs spread over
~1.8 KB of guest code, not one address, so the guest is executing a real loop.

*"We return the wrong locus value."* `ScePlayGoLocus` is one byte; we were writing a `u32` per entry into a
caller's byte array on the stack, smashing three bytes after it — a real bug, found because the out-pointer
in the trace was **unaligned** (`0x4002130f9`), which only makes sense for a `char`. Fixing it did **not**
stop the loop. Then a sweep of the locus value (0/1/2/4) across four runs: no value changed anything.

**What it actually was.** The loop is driven by the **return code**, not by the data. Following the stub map
(§3.8): `scePlayGoGetLocus` sits at `0x20001b60`; exactly one GOT slot in the eboot holds that address
(`0x4d52228`); one PLT entry jumps through it; one `call` reaches that PLT. The comparison immediately after
the call is the loop's exit:

```
de12a0:  call   scePlayGoGetLocus
de12a5:  mov    %eax,%ecx
de12a9:  cmp    $0x80b2000c,%ecx
de12af:  je     0xde12fb          ; loop exit
...
de12dc:  call   0xe1a420          ; the bit-scan/hash routine the watchdog kept sampling
```

The title enumerates chunk ids until one is **refused**, because that is the only way an enumerating caller
can discover where a table ends. We answered success for every id, so it never found one — and did real
hash-table work on each iteration, which is why the hot RIP sat in a routine with no `call` in it.

**Fix.** A complete local dump is not chunked: exactly one chunk (id 0) holding everything, and any id past it
returns `0x80b2000c` with nothing written. Result: unbounded grind → next wall reached in **0.795 s**,
`sceKernelOpen` 4 → 7.

**Transferable lessons.**
- A syscall called millions of times a second with no I/O and no frames is **enumeration**, not a spin. Check
  the return-code comparison at the call site before theorising about the data.
- "Always succeed" is not a safe default for any query a caller uses to find a boundary. Refusing is
  information; a stub that cannot refuse removes the only signal the caller has.
- Sweeping a returned VALUE and getting no change is itself evidence: it points at the return code.
- A field width the header never states is a real hazard. An unaligned out-pointer is the tell that a field is
  a byte, and writing four smashes the caller's neighbouring state (same shape as case 30's sibling defect in
  `sceLibcHeapGetTraceInfo`, which faulted before `main`).

---

## Case 29 — a symbol was "missing" while the module exporting it sat loaded three lines earlier: a dependency CYCLE, wearing two disguises (task-29)

**Symptom.** The UE4 title stubbed `malloc`, `memcpy`, `free`, `strlen` and `printf` as MISSING while shipping
a `libc.prx` that exports every one of them. Later, `sceFiosFHOpenWithModeSync` was reported missing with
`libSceFios2` loaded and its dumped `.map` listing the symbol at a real address.

**Disguise 1 — a module name is not a library name.** The SCE dynamic format carries two namespaces, and we
were feeding the dependency walk the wrong one: `DT_SCE_NEEDED_MODULE` entries were parsed and then discarded
(`let _ = &module_entries;`), so the loader used LIBRARY names as file names. `libSceFios2` needs library
`libSceLibcInternal`; the file that provides it is `sce_module/libc.prx`. Looking for
`libSceLibcInternal.prx` found nothing, the loader concluded "HLE-provided", and every libc import in Fios2
became a permanent missing stub — permanent because a relocated GOT is never revisited.

**Disguise 2 — one file, two names, two mappings.** With an alias in place, `libc.prx` was mapped **twice**,
at two bases, with two copies of its globals and two `module_start` runs. Name-keyed dedup cannot see this.
Both the cycle-breaking set and a process-wide registry now key by PATH — the registry because a runtime
`sceKernelLoadStartModule` walk starts with an empty set and would otherwise re-map what boot already mapped.
This second defect predated the change and was masked by names that happened to agree; it surfaced as
**Celeste** regressing, caught by re-running the working title rather than by any test.

**The real shape underneath.** `libSceLibcInternal` and `libSceFios2` import **each other**. Post-order load
works on a DAG; a cycle has no topological order, so whichever module relocated first was relocated against a
world where the other did not exist.

**Fix.** Split loading in two: `map_image` (allocate a base, map segments, REGISTER the module and its
exports) for the whole graph, then `relocate_image` for all of it. By the time any relocation resolves a
symbol, every module's exports are registered — the cycle stops being a question rather than being answered.
Found on the way: export registration used to run *after* relocation, so a module could not resolve even its
own symbols while relocating.

**Transferable lessons.**
- "Symbol missing, exporter loaded" is the fingerprint of an ordering problem, not a resolution bug.
- If a graph can contain cycles, no ordering fixes it — separate the phase that publishes names from the
  phase that consumes them.
- Dedup identity matters: key on the FILE, not on the name a caller happened to use.
- The only regression oracle for a loader change is the title that already worked. Run it every time.

---

## Case 30 — an import whose NAME does not exist anywhere: implemented from its caller, bound by its hash

**Symptom.** `[FATAL] missing symbol: fJgP+wqifno`. A NID with no name — absent from `data/ps4_names.txt`
(94,276 names) and therefore unresolvable, because a NID is a one-way SHA-1 of the name.

**What did not work.** A dictionary attack: ~134,000 candidates over generic and then LBA-focused vocabulary,
after confirming the hash function against a known pair (`sceDiscMapIsRequestOnHDD` → `lbQKqsERhtE`). No
preimage. Recorded here so nobody spends the hour again.

**What did.** The import record carries the **library** as well as the hash, and we were discarding it. One
line of linker output turned an unidentifiable wall into a tractable one:

```
[LINKER] Stubbed missing: fJgP+wqifno (unnamed NID, from libSceDiscMap)
```

From there the stub-map walk (§3.8), with the twist that the importer is a **sibling module**, not the eboot:
`libSceFios2` holds the GOT slot, one PLT jumps through it, two call sites reach that PLT. The caller
describes the function completely:

```
52c4f7:  lea    -0x38(%rbp),%rcx      ; three out-pointers
52c4fb:  lea    -0x40(%rbp),%r8
52c4ff:  lea    -0x48(%rbp),%r9
52c50b:  call   <this>
52c515:  test   %ecx,%ecx             ; non-zero = failure
52c519:  mov    -0x38(%rbp),%rsi
52c51d:  cmp    $0x2,%rsi
```

and the sibling call site logs, on the unexpected branch:

```
"FIOS2 WARNING: An unexpected value of %ld was returned for the LBA package location"
```

Three out-values, 0 on success, first value expected to be 0 or 1, and the string names the job. Nothing we
serve has a disc layout, so 0 is the truthful answer.

**The mechanism it needed.** Handlers were bindable only by name, since the export registry hashes names to
reach NIDs. `#[ps4_syscall(..., nids = ["fJgP+wqifno"])]` now declares raw NIDs a handler answers to, with a
synthetic syscall id above the generated range. Explicit NIDs `insert` rather than `entry`: a hand-written
hash is a deliberate statement about which import is served and should beat anything inferred from a name.

**Transferable lessons.**
- You never need the name. You need the behaviour, and the caller has it — pre-zeroed out-parameters, the
  value it range-checks, and above all any string it logs on the unexpected branch.
- Print the library for every unresolved import. It costs one line and is the difference between "a hash" and
  "one small API".
- When a lookup is one-way, stop trying to invert it and go around.

## Case 31 — a title deadlocked at boot on a GPU-completion signal the WORKING title needs withheld; the diagnosis was tool-driven and the first fix regressed the working title (task-157 follow-up)

**Symptom.** Little Nightmares (UE4, CUSA05952) boots through its whole import wall and then
hangs: 30 threads parked, no frames advancing, no fault, forever. Nothing printed.

**Diagnosis — four cheap tools, in order, each naming the next.** ptrace is blocked in this
environment, so every step is in-emulator instrumentation (doc-4 §3.7, §3.10):

1. The profiler's **in-flight syscall list**, now carrying guest **thread names**, showed
   `SubmitDoneAsyncTaskThreadPS4` blocked in `scePthreadMutexLock` for 54 s. "tid 27 is stuck"
   is noise; "the submit-done thread is stuck" is a lead.
2. The **`[SYNC]` stuck-lock reporter** (fires when a guest mutex is held past 5 s) named the
   holder: *tid 1 (Thread-1) has held mutex 0x4d72d78 for 54 s*. Now we have the culprit, not
   the victim.
3. **exectrace's "last syscall before the silence"** on tid 1: `sceGnmSubmitCommandBuffers`,
   42 s ago, nothing since. And its RIP histogram was **99% one address** — a tight spin inside
   guest code, immediately after a submit.
4. So tid 1 submitted, then spun waiting for that submit to *complete*, while holding a lock the
   engine's own submit-done thread was blocked on. A GPU-completion wait that never completes.

**The one-line experiment that proved it.** task-157 withholds the inline EOP memory-fence
label (Celeste needs it withheld, case 24). `UNEMUPS4_GPU_EOP_SYNC=1` restores the inline
write. With it set, Little Nightmares un-wedged instantly and walked to its next wall. So this
title polls the EOP label in guest memory as its **only** completion signal — the exact
opposite of Celeste, which reads completion from the equeue and collapses to white textures if
the label is written (case 24).

**The first fix — and how it regressed the working title.** The obvious discriminator: write
the label only for a title that does not use the equeue. Gated on whether the guest had ever
**waited** on an equeue, set on the first `sceKernelWaitEqueue`. It un-wedged Little Nightmares.
It also turned Celeste's splash into **white boxes with debug text** — the case-24 collapse,
back. The flag is decided too late: Celeste has not reached its first equeue wait during its
first ~3 submits, so those boot frames got the label written, and three collapsed frames are
enough for gnmx to enter its recycle-skip path and stay there. Caught only by the maintainer's
eyes on a real frame — the logs said nothing (doc-4: a visual bug needs a visual oracle).

**Why registration cannot be the discriminator either.** Both titles call `sceGnmAddEqEvent` at
boot. Little Nightmares registers an equeue event and then never waits on it once — it polls the
label. At the first submit, the moment the decision must be made, the two titles are **genuinely
indistinguishable**: both registered, neither has waited.

**The fix that holds.** Default WITHHOLD, unconditionally — correct for Celeste from frame 0.
Write the label only once a title has **positively proven** it is a poller: no equeue completion
ever collected, AND past a 1 s boot grace since its first EOP submit. An equeue title trips the
"collected a completion" flag within milliseconds, deep inside the grace, so it withholds on
every frame including boot. A poller never collects, so after the grace it writes and un-wedges
— a ~1 s boot stall only it pays. The flag is set from `sceKernelWaitEqueue` (the collection),
not `sceGnmAddEqEvent` (the registration).

**Confirmation.** Celeste textured on screen (maintainer's eyes). Little Nightmares 2 → 7
presented frames and a live flip thread. A regression test drives both arms plus the boot-frame
case (equeue title, grace elapsed → still withheld — the exact case the screenshot broke),
planting the timestamp through a test hook so it needs no sleep.

**Transferable lessons.**
- The diagnosis was a chain of four tools, each of which named the input to the next: in-flight
  list (with thread names) → stuck-lock holder → last-syscall-before-silence → RIP spin. None
  needed ptrace. Build the tool that names *who holds the lock*, not just *that a lock is held*.
- When two guests want opposite things from one mechanism, do not hunt for a switch readable at
  decision time — sometimes there isn't one. Default to what cannot harm the working guest, and
  switch away only on positive proof that accrues over time.
- A safety-critical per-frame default must be right from frame 0. "Right once a late signal
  confirms it" spends the boot frames — the most expensive ones — on the wrong branch.
- A grace threshold picked as *"longer than the working title's first-wait latency"* is
  structural, not a tuned timeout; it only ever delays a guest that was going to take the other
  branch anyway.
