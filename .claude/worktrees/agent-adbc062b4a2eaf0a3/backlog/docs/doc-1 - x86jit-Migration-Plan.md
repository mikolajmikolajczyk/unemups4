---
id: doc-1
title: x86jit Migration Plan
type: other
created_date: '2026-07-09 15:05'
---


## Context

unemups4 (PS4 HLE emulator) wykonuje dziś kod gościa **natywnie na hoście**: `launch.s` przełącza FS base przez `arch_prctl`, skacze do ELF-a; importy/syscalle idą przez 32-bajtowe stuby (`MOV EAX,id; MOV R11,tramp; CALL R11`) → `trampoline.s` → `rust_syscall_handler`. Cel: przełączyć wykonanie CPU na bibliotekę **x86jit** (`/home/mikolaj/src/x86jit`, path dep) — guest-agnostic silnik x86-64 (interpreter + Cranelift JIT, `Vm`/`Vcpu`/`Exit::Syscall`, FS/GS base, Jaguar ISA `GuestCpuFeatures::v2()`). Zysk: kontrolowane wykonanie (koniec z natywnym skakaniem po hoście), przenośność (ARM64), SMC handling, lepsze diagnozy, docelowo JIT ~natywna prędkość.

Po zatwierdzeniu planu: taski trafiają do Backlog.md (task-per-milestone poniżej).

## Decyzje projektowe (zatwierdzone)

1. **Identity mapping zostaje** — host addr == guest addr. Wymaga zmiany w x86jit (user zatwierdził): `guest_base: u64` w `HostRam`/Reserved, translacja `host = ptr + (g - guest_base)` (arytmetyka na usize — nie materializować null-adjacent pointera). unemups4 mmapuje `MAP_FIXED_NOREPLACE|MAP_NORESERVE` na `0x10000`, span 64 GiB. Wszystkie dereferencje guest pointerów w handlerach/GPU działają bez zmian.
   - Korekta faktów: lazy stuby NIE są na 0x0 — `LinuxMemoryManager::map(0,...)` = "allocate anywhere" → heap cursor od `0x4_0000_0000`. Stąd span ≥ 17 GiB → 64 GiB NORESERVE.
2. **Stuby syscalli** → `49 89 CA B8 <id> 0F 05 C3` (`MOV R10,RCX; MOV EAX,id; SYSCALL; RET`, pad NOP do 32 B). Lift x86jit jest hardware-correct: SYSCALL clobberuje RCX (<-RIP) i R11 (<-RFLAGS), więc 4. arg call-ABI (RCX) nie przeżywa trapu. Stub kopiuje RCX → R10 (rejestr 4. argu syscall-ABI, nietykany przez SYSCALL) przed SYSCALL, a `NativeContext.arg3()` czyta R10. Marker missing-symbol `0xC000_0000` bez zmian.
3. **`crates/cpu` = safe wrapper na x86jit** — krawędzie zależności (kernel/libs → cpu) stabilne. `NativeContext` przenosi się do ps4-cpu, re-export z ps4-libs. Dispatch przez globalny `OnceLock` callback (unika cyklu cpu→libs). Nested calls (TLS dtors, pthread_once) przez HLT-gadget: push adresu gadgetu jako return address, run do `Exit::Hlt`, RAX = wynik.
4. **Wątki**: host thread per guest thread, `Vcpu` per wątek nad `Arc<GuestVm>` trzymanym przez `Process`. `Reg::FsBase = tls_base` — koniec z arch_prctl, host FS nietykany, Rust TLS działa naturalnie.
5. **Pamięć**: nowy `VmMemoryManager` (trait `VirtualMemoryManager` bez zmian). Cały arena pre-mapped raz (`Vm::map` jest `&mut self`; potem Arc). Runtime map/unmap = software VMA (BTreeMap jak dziś) + `madvise(DONTNEED)`. **`write_bytes` przez `vm.write_bytes`** — SMC tracking widzi zapisy loadera/handlerów. Goblin loader zostaje; x86jit-elf NIE używany (PS4 NIDs specyficzne).
6. **GPU**: bez zmian pod identity — tylko weryfikacja (display.rs:140, vulkan.rs:594).
7. **Staging**: bez cargo feature flag — big-bang w kolejności: taski 1-4 budują nową ścieżkę OBOK starej (wszystko się kompiluje, native działa), task 5 przełącza, task 8 kasuje. Oracle = stdout baseline'y 6 przykładowych ELF-ów zebrane przed przełączeniem.
8. **Wydajność**: interpreter najpierw, Cranelift po poprawności. `MemConsistency::Fast` (x86 host: identyczny kod we wszystkich tierach), `GuestCpuFeatures::v2()`, budget None (syscall exits = punkty kooperacji; exit flag same-thread).

## Taski (→ Backlog.md)

### Task 1 — Baseline'y natywne + path dep x86jit
- `Cargo.toml` workspace: `x86jit-core = { path = "../x86jit/x86jit-core" }`, `x86jit-cranelift` analogicznie (jeszcze nieużywane).
- Nowy `scripts/run_examples.sh`: każdy z 6 ELF-ów (`examples/ps4-helloworld/hello_world.elf`, `ps4-fs.elf`, `ps4-mmap.elf`, `ps4-tls.elf`, `ps4-thread-testing.elf`, `ps4-softgpu.elf`) z timeoutem, stdout+exit code → `scripts/baselines/*.txt`; normalizacja niedeterminizmu (TID/timestampy) sedem. Commit baseline'ów.
- **AC**: skrypt produkuje baseline'y; powtórny run diffuje czysto; `cargo build --release` zielony.

### Task 2 — SPIKE (keystone): `guest_base` w x86jit
- Repo x86jit: `x86jit-core/src/memory.rs` (`HostRam.guest_base: u64` default 0; translacja `ptr + (g - guest_base)` na usize; reject map poniżej guest_base), cranelift codegen (baked numeric base), testy różnicowe z `guest_base=0`.
- Wzorzec mmap: `x86jit-linux/src/hostmem.rs::reserve`, ale `mmap(0x10000, span-0x10000, RW, PRIVATE|ANON|NORESERVE|MAP_FIXED_NOREPLACE)`.
- **AC**: test — Reserved VM z `guest_base=0x10000`, map 0x400000, write `mov eax,42; hlt`, run → Hlt, RAX==42, ORAZ `unsafe { *(0x400000 as *const u8) } == 0xB8` od strony embeddera (identity udowodnione). Suita x86jit zielona: interpreter + cranelift.
- **Ryzyko**: dotyka hot path pamięci. Gate'uje wszystko — robić pierwsze (równolegle z Task 1).

### Task 3 — Rework `crates/cpu` na wrapper x86jit (stary asm zostaje, nieużywany)
- Nowe: `guest_vm.rs` (`GuestVm::new(span)`: identity mmap, `Vm::with_backend_host_ram`, pre-map RWX/Ram całej areny, HLT gadget page np. guest 0x30000, `v2()` features, potem `Arc`), `exec.rs` (run loop), `context.rs` (przeniesiony z libs).
- API: `set_syscall_dispatch(fn(u64, &mut NativeContext) -> u64)` (OnceLock); `run_guest_call(vm, entry, rsp, rdi, fs_base) -> GuestExit{Returned(rax)|ThreadExit(val)|Fatal(String)}`; `call_guest(entry, arg)` (nested, fresh Vcpu na `cur_rsp - 128` z thread-local exec contextu); `request_thread_exit(value)` (zastępuje should_exit/VmStateAbi).
- Run loop: `Exit::Syscall` → NativeContext z 15 GPR-ów → dispatch → `set_reg(Rax, ret)` → check exit flag; `Hlt` przy gadget+1 → `Returned(rax)`; reszta → `Fatal` z kontekstem (rip, addr, hexdump `UnknownInstruction`).
- **AC**: `cargo test -p ps4-cpu` — ręcznie asemblowany guest: (a) `mov eax,42; ret` → 42; (b) stub SYSCALL dispatchuje, wszystkie 6 argów czytelne (RCX!), return w RAX; (c) nested call_guest; (d) request_thread_exit(7) → ThreadExit(7). Workspace nadal się buduje (native nietknięte).

### Task 4 — `VmMemoryManager` w crates/memory
- `vm_backend.rs`: port VMA BTreeMap + find_free_region + heap cursor z `linux.rs`; `map` = collision check + VMA insert (bez mmap — arena gotowa); `unmap` = VMA remove + madvise; `protect` = tracking-only (jak dziś efektywnie); `get_host_ptr(addr) = addr` (guard: w spanie); `write_bytes/read_bytes` → `GuestVm::write_bytes/read_bytes` (SMC). Reject < 0x10000 / > span.
- **AC**: `cargo test -p ps4-memory` — round-tripy map/write/read/unmap; identity (`*(0x400000)` po write); kolizje/out-of-span błędzą.

### Task 5 — PRZEŁĄCZENIE: stuby SYSCALL + wiring + run loop wątku (interpreter)
- `kernel/src/hle.rs` + `loader/src/linker.rs:152-207`: emiter stubów → `B8 <id> 0F 05 C3`; drop trampoline_addr/set_trampoline.
- `app/main.rs`: `Arc<GuestVm>` + `VmMemoryManager` zamiast `LinuxMemoryManager`; delete trampoline plumbing (linie 72-74); `set_syscall_dispatch(rust_syscall_handler)`; `Process.guest_vm` (nowe pole).
- `kernel/thread.rs:36-96`: `execute()` → `run_guest_call`; main thread `rdi = start_rsp`; `pthread.rs:73-87`: `sce_pthread_exit` → `request_thread_exit`.
- Stray `syscall` w kodzie gry trapuje do dispatchera zamiast host kernela — bezpieczniej.
- **AC**: hello_world, ps4-fs, ps4-mmap — stdout diff czysty vs baseline'y. Missing-symbol path nadal daje `[FATAL ERROR] ... missing symbol`.
- **Ryzyko**: `Exit::UnknownInstruction` na instrukcjach Orbis CRT → małe dodatki do liftu x86jit; budżet 1-2 iteracje.

### Task 6 — Wątki, TLS, nested calls
- `thread.rs:113-162` (dtory przez `call_guest`), `pthread.rs:193-233` (`sce_pthread_once` przez `call_guest`); worker `RDI = entry_argument`; reset exit flag przed dtorami.
- **AC**: `ps4-thread-testing.elf` i `ps4-tls.elf` zgodne z baseline'ami; dtory odpalają (RUST_LOG=debug).

### Task 7 — Weryfikacja GPU
- Bez zmian kodu; run `ps4-softgpu.elf` — framebuffer pisany przez JIT-owane story, display loop czyta identity pointerami.
- **AC**: softgpu renderuje jak natywnie (screenshot do `screenshots/`); stdout zgodny; zero Fatal.

### Task 8 — Kasacja natywnego backendu
- Delete: `launch.s`, `trampoline.s`, `vmstate.rs`, `global_asm!`, `memory/src/linux.rs`, arch_prctl w `thread.rs:47-51`, trampoline fields w linkerze. Scrub README (opisuje natywny HLE + caveat "syscall idzie do host kernela" — nieaktualny), AGENTS.md, backlog/docs/architecture.md + glossary.md (trampoline/FS-swap → x86jit).
- **AC**: `grep -rn "cpu_launch_guest\|syscall_trampoline\|VmStateAbi\|arch_prctl" crates/ app/` puste; build zielony; 6 przykładów zgodne z baseline'ami.

### Task 9 — Cranelift JIT + tier-up + skrypt różnicowy
- `x86jit-cranelift` dep; wybór backendu przez env var `UNEMUPS4_BACKEND=interp|jit` (jeden binarek, zero feature-matrix); `set_tier_up_after(~50)`, `set_tier_up_background(true)`. Guard-pages: NIE teraz (arena RW pre-mapped; out-of-span SIGSEGV jak dziś natywnie) — follow-up task.
- Nowy `scripts/diff_backends.sh`: każdy przykład pod oboma backendami, diff stdout.
- **AC**: 6 przykładów zgodne pod JIT; diff_backends czysty; hello-world wall time < ~2x natywnego (sanity).

### Task 10 — Diagnostyka
- `UnmappedMemory` → RIP + access + nazwy VMA z memory managera; `Exception` → nazwy sygnałowe; `UnknownInstruction` → disasm okolicy + hint "zgłoś do x86jit"; opcjonalny budget watchdog za env var (RIP co N bloków przy debugowaniu zawieszek).
- **AC**: null-deref w teście daje raport z RIP/addr/VMA; `ud2` daje raport Exception; przykłady bez regresji.

## Ryzyka
- **Task 2 = keystone** — hot path pamięci x86jit; mitygacja: testy różnicowe guest_base=0, suita Unicorn-differential zielona przed merge w x86jit.
- **Pokrycie liftu dla Orbis CRT** — niewiadoma do pierwszego runu; `UnknownInstruction` raportuje bajty; sufit v2 ISA ogranicza powierzchnię.
- **Host pointer do gościa**: `main.rs:83` podaje `&ARGC_ZERO` (host static) jako entry_arg — ignorowane dla main, ale w Task 5 grep handlerów pod kątem host-pointerów zwracanych gościowi.
- **Kolizja MAP_FIXED_NOREPLACE [0x10000, 64 GiB)**: dziś MAP_FIXED na 0x400000/0x20000000/0x4_0000_0000 działa → zakres wolny; mmap głośno assertuje przy boocie.
- **`Vm::map` jest `&mut self`**: pre-map omija; przyszłe runtime Trap regions (GPU MMIO?) będą wymagać `&self`-map w x86jit — poza zakresem, odnotowane.

## Weryfikacja end-to-end
Oracle: baseline'y stdout z Task 1. Po każdym tasku od 5 wzwyż: `./scripts/run_examples.sh` + diff. Task 9 dodaje różnicowanie interp-vs-JIT. Finalnie: 6/6 przykładów zgodne pod oboma backendami, natywny backend usunięty, docs zaktualizowane.

## Po zatwierdzeniu
Utworzyć taski w Backlog.md (`backlog task create`, devShell) — 10 tasków wg powyższego, task 2 oznaczony jako spike/keystone, zależności: 2→3→4→5→6→7→8→9→10, task 1 równolegle.
