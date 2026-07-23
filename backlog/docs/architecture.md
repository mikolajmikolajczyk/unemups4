# Architecture

Repo shape, data flow, key modules. Keep this **descriptive of the current state**, not aspirational. For decisions about why the architecture is what it is, see [`decisions.md`](decisions.md) / `ls backlog/decisions/`.

## Layout

Cargo workspace (`resolver = "2"`), Rust edition 2024. Members: `app/unemups4` and `crates/*`.

```
app/unemups4/     Emulator entrypoint (boot + display loop). Crate name `unemups4`.
crates/
  core/           ps4-core — core traits + kernel/GPU/dirty registration; KernelInterface, DirtySource, GpuBackend
  cpu/            ps4-cpu — x86-64 guest execution core; wraps x86jit Vm/Vcpu; VmDirtySource
  memory/         ps4-memory — UMA memory backend
  loader/         ps4-loader — ELF loader + dynamic linker (goblin)
  kernel/         ps4-kernel — HLE: process, filesystem, TLS; provides KernelBridge
  syscalls/       ps4-syscalls — syscall table (generated at build time by build.rs)
  libs/           ps4-libs — libkernel / system library emulation
  gcn/            ps4-gcn — GCN ISA decoder, CPU wave interpreter (correctness oracle), GCN→SPIR-V recompiler (Vulkan-free)
  gnm/            ps4-gnm — PM4 command processor, GPU shadow state, shader cache (Vulkan-free)
  gpu/            ps4-gpu — Vulkan presentation backend (AshBackend); only crate allowed to use ash/winit
  macros/         ps4-macros — proc-macro helpers (#[ps4_syscall])
examples/         Homebrew C/C++ test programs; each ships a prebuilt .elf
game_data/        Guest mounts: app0/ → /app0, system/ → /system
data/             ps4_names.txt, wiki_syscalls.txt; oo_sdk/ cloned by user (not vendored)
backlog/          tasks, docs, decisions (this tree)
scripts/          dev helpers (skills-bootstrap, uid-guard)
```

## Data flow

Guest code runs on the **x86jit** x86-64 engine, not natively on the host CPU. The default backend is the Cranelift JIT with background tier-up; `UNEMUPS4_BACKEND=interp` switches to the plain interpreter (used as a differential oracle). `crates/cpu` wraps x86jit's `Vm`/`Vcpu` behind a small safe API (`GuestVm`, `run_guest_call`, `call_guest`, `set_syscall_dispatch`). The guest arena is **identity-mapped**: `GuestVm::new` mmaps a single `MAP_NORESERVE` span with x86jit `guest_base` set so a guest address is the same number as the host address, and guest pointers remain valid host pointers — handlers and the GPU dereference them directly. `crates/memory`'s `VmMemoryManager` is a software VMA table over that pre-mapped arena (`map`/`unmap` are bookkeeping + `madvise`; writes route through `GuestVm::write_bytes` so x86jit's SMC tracking sees them).

Imported library functions resolve to generated `MOV EAX, id; SYSCALL; RET` stubs. The `SYSCALL` traps out of the guest as `Exit::Syscall`; the run loop in ps4-cpu marshals the 15 guest GPRs into a `NativeContext`, calls the globally-installed dispatch fn (`rust_syscall_handler`), and writes the result back into `RAX`. Handlers register themselves via the `#[ps4_syscall]` macro + the `inventory` crate; dispatch is a flat id→handler table. A stray `syscall` in guest code therefore traps to the emulator, never to the host kernel. Each guest thread is a host thread one-to-one, running its own `Vcpu` over a shared `Arc<GuestVm>`; per-thread TLS is installed into the Vcpu's `Reg::FsBase` (host FS untouched). Nested guest calls (TLS destructors, `pthread_once`) use a HLT gadget as the return address (`call_guest` runs a fresh Vcpu until `Exit::Hlt`). Guest mutex/condvar map onto host primitives keyed on the guest object address.

**GPU pipeline.** `crates/gnm` (Vulkan-free) is the PM4 command processor. It decodes PM4 packets from guest command buffers, maintains a submit-spanning shadow register file (`GpuState` / `RegFile`, sparse per-bank maps), handles EOP/EOS GPU→CPU sync label writes, and resolves bound shader pairs (embedded vs. real `.sb`) at draw time. The `Executor` runs on the guest thread (inside the `libSceGnmDriver` HLE handlers) and emits `BackendCmd`s over a channel to the display thread. The display thread owns the Vulkan device and state (`AshBackend` in `crates/gpu`). Two GPU paths exist: (1) *software framebuffer* — guest renders into its own memory, host blits that into a Vulkan texture on a full-screen quad; (2) *GNM/PM4 draw* — for draws bound to firmware-embedded shaders (`EmbeddedShaderProvider`), the executor sends `BindEmbeddedPipeline` + `DrawAuto` backend commands, and for draws bound to real guest shaders the `.sb` OrbShdr container parser (`crates/gnm/src/shader/sb.rs`) feeds the GCN ISA decoder and GCN→SPIR-V recompiler in `crates/gcn` (with the CPU wave interpreter as the correctness oracle). `BackendCmd` carries the buffer/texture-upload and pipeline-create/bind variants (`PipelineId`, `CreatePipeline`, `BindPipeline`); T#/V#/S# descriptor decode (`crates/gnm/src/vbuf.rs`), a texture cache, tiling/detile, and multi-texture pixel-shader binding all work. This full path is landed and exercised by retail: Celeste renders in-game through PM4 → GCN decode → interpret/recompile → Vulkan, to gameplay. Keyboard maps to a virtual DualShock via `scePad`.

**Dirty tracking.** The `DirtySource` seam (declared in `ps4-core`, implemented in `ps4-cpu` as `VmDirtySource`) exposes the x86jit watched-range facility to the `ResourceCache` in `ps4-gnm` without creating a gnm→cpu dependency. The resource cache polls it at submit boundaries to skip re-uploads of guest ranges that the JIT has not written since the last submit. An `AlwaysDirty` fallback (forced via `UNEMUPS4_DIRTY=always` or in headless tests) re-uploads every submit, keeping correctness at the cost of performance.

## Layering rules

To keep the crate graph acyclic, the `KernelInterface` trait lives in `core`, `libs` calls it, and `kernel` provides the concrete `KernelBridge`. `ps4-cpu` depends on `x86jit-core` (the execution engine) and is depended on by `kernel`, `memory`, and `libs` (via the re-exported `NativeContext`); syscall dispatch is wired at runtime through a `OnceLock` callback (`set_syscall_dispatch`) to avoid a cpu→libs cycle.

GPU layering (verified via Cargo.toml deps): `ps4-core` ← `ps4-gcn` ← `ps4-gnm` ← `ps4-gpu`. Only `ps4-gpu` may use `ash`/`winit`/`gpu-allocator`; `ps4-gnm` and `ps4-gcn` are Vulkan-free and headless-testable. Cross-thread GPU seams (`PresentSink`, `GpuBackend`, `DirtySource`, `register_present_sink`) live in `ps4-core` so `ps4-gnm` can reach them without depending on `ps4-gpu` or `ps4-cpu`. Not currently enforced by a lint tool (`cargo deny` not configured).
