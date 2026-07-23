---
id: doc-7
title: Retail bring-up casebook — worked debugging examples
type: other
created_date: '2026-07-15 04:35'
---

# Retail bring-up casebook — worked debugging examples

Concrete case studies from a real bring-up (a Mono/MonoGame title, from "dies on the first
imported call" to "the managed runtime loads the game assembly"). Each case is one wall:
the **symptom** as it appeared, the **tool** that located it, the **reasoning**, the
**fix**, and the **lesson**. Read doc-6 first for the method and the toolbox; this is the
pattern library that makes the method concrete.

Each case is deliberately shown as *the reader would experience it* — the wrong first guess
is kept where it happened, because avoiding the wrong guess is the teachable part.

> **Maintaining this casebook (do this as you go).** This document is only useful if it keeps
> pace with the bring-up. When you clear a wall whose lesson **generalizes** — a new *shape*
> of bug, a new tool, a non-obvious cause — add a case here in the same
> symptom → tool → diagnosis → fix → lesson format, as part of that fix's commit. Keep the
> wrong first guess if there was one. Do **not** add a case for every routine missing-symbol
> stub; add one when a future reader would save real time from the story. If the wall is a
> new *class* (not just a new instance), also add a row to doc-6's taxonomy table (§4).

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

**Tool.** The **guest backtrace** (doc-6 §3.2). This is the case that justified building it.

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

## The through-line

Across all ten cases the move is identical: **turn the symptom into a precise cause with a
tool before touching code.** A raw-bytes dump beat a wrong theory (Case 1); a full
program-header enumeration found an unmapped segment type (Case 2); a guest backtrace named a
deliberate abort's caller (Case 3); a relocation read explained a null (Case 4); an assertion
string named a missing fact (Case 5); gdb caught a crash the run loop couldn't (Case 6);
logged arguments exposed a `len=0` (Case 7). The tools in doc-6 §3 exist to produce exactly
these facts. Guessing is slow and often wrong; a backtrace, a gdb stop, a logged argument, or
an `xxd` is fast and certain. Reach for the instrument first.
