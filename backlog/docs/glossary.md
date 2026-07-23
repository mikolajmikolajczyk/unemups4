# Glossary

Domain and project terminology. One term per entry. Keep definitions short — link out for depth.

## PS4 / emulation domain

- **HLE (High-Level Emulation)** — reimplementing the system *around* the guest (libraries, syscalls) in host code instead of emulating hardware. unemups4's entire design: guest CPU code runs on the x86jit engine, and only the OS surface is faked in Rust.
- **Orbis OS** — the PS4's operating system, a FreeBSD 9 derivative. Its kernel/syscall semantics are what `ps4-kernel` and `ps4-syscalls` emulate.
- **Homebrew** — unofficial, user-built PS4 software (plain unencrypted ELFs). The only thing unemups4 targets — retail games won't load.
- **sce\* / libSce\*** — Sony's prefix for system functions and libraries (`sceKernel*`, `scePad*`, `sceVideoOut*`, `sceUserService*`). Emulated per-library under `crates/libs/src/lib*/`.
- **NID (Name IDentifier)** — Sony's symbol hash used instead of names in PS4 dynamic linking: SHA-1 of the symbol name + a fixed salt, first 8 bytes, base64-encoded. Computed by `calculate_nid` in `crates/syscalls/build.rs`.
- **SELF / fSELF** — Signed ELF (and fake-signed variant): Sony's encrypted/signed executable container. **Not supported** — only plain ELF loads (via `goblin`).
- **PRX** — a PS4 dynamic library module (`.prx`/`.sprx`). Multi-prx linking is reduced to the single-module case here.
- **DT_SCE_\*** — Sony-specific dynamic-section tags in PS4 ELFs (relocation/NID tables). Not implemented; one reason retail binaries won't load.
- **eboot.bin** — the conventional filename of a PS4 app's main executable.
- **OpenOrbis (PS4 Toolchain)** — open-source homebrew SDK. Cloned into `data/oo_sdk/` (not vendored) so `crates/syscalls/build.rs` can extract syscall argument metadata; also what builds the C/C++ programs in `examples/`.
- **`#[ps4_syscall]`** — proc-macro from `ps4-macros` that marks a Rust function as the handler for a guest syscall/library import and registers it via `inventory`. The way to implement a new handler — never hand-edit a dispatch table.
- **inventory registration** — compile-time distributed registry (the `inventory` crate) that collects all `#[ps4_syscall]` handlers into the flat id→handler dispatch table.
- **x86jit** — the guest-agnostic x86-64 execution engine (`Vm`/`Vcpu`, interpreter + Cranelift JIT, path dep `x86jit-core`) that runs guest code. Wrapped by `crates/cpu`; guest syscalls surface as `Exit::Syscall`.
- **GuestVm** — `ps4-cpu`'s wrapper around an x86jit `Vm` plus the single identity-mapped guest arena. Shared via `Arc`; each guest thread runs its own `Vcpu` over it. Owns the run-loop helpers (`run_guest_call`, `call_guest`).
- **Exit::Syscall** — the x86jit `Vcpu::run` exit raised when guest code executes `SYSCALL` (from an import stub or a stray instruction). The ps4-cpu run loop marshals the guest registers into a `NativeContext`, dispatches to a Rust handler, and resumes — the guest never reaches the host kernel.
- **HLT gadget** — a one-byte `HLT` at a fixed guest page, pushed as the return address for a nested guest call (`call_guest`). Returning from the callee hits it, raising `Exit::Hlt`, which ends the nested run and yields `RAX`. Used for TLS destructors and `pthread_once`.
- **FS base** — the x86-64 FS segment base register, which anchors TLS. The guest's TLS base is set per-thread in the Vcpu's `Reg::FsBase`; the host FS is never touched, so Rust TLS in handlers works normally.
- **Identity mapping (`guest_base`)** — the guest arena is mmapped once (`MAP_NORESERVE`) with x86jit's `guest_base` set so a guest address *is* the host address and guest pointers are valid host pointers. No address translation on the hot path.
- **UMA (Unified Memory Architecture)** — the PS4's single memory pool shared by CPU and GPU. Emulated by `ps4-memory`.
- **GNM / GnmDriver** — the PS4's low-level graphics API (Sony's console equivalent of Vulkan). The PM4 path and embedded-shader draw work; the GCN shader path runs real `.sb` blobs end to end — retail Celeste renders in-game through PM4 → GCN decode → interpret/recompile → Vulkan.
- **Liverpool** — codename of the PS4's AMD APU/GPU; "Liverpool command processing" means consuming its PM4 command stream. The PM4 decoder is implemented and real GCN shader execution runs — the CPU wave interpreter and GCN→SPIR-V recompiler in `crates/gcn` execute guest shaders through the live draw path.
- **OrbShdr / `.sb`** — the OpenOrbis/Sony shader-binary container format: GCN machine code followed immediately by a packed 28-byte `ShaderBinaryInfo` header with the magic `"OrbShdr"`. Parsed by `crates/gnm/src/shader/sb.rs`.
- **ShaderBinaryInfo** — the 28-byte header appended after GCN machine code in an OrbShdr `.sb` blob: contains the 7-byte magic, shader stage (`m_type`), code length (`m_length`), and pointers to semantic tables. Parsed by `parse_sb`.
- **RegFile / GpuState** — `RegFile` is a sparse shadow of one PM4 register bank (CONTEXT/SH/UCONFIG/CONFIG). `GpuState` owns all four banks plus the bound-shader table; it's the authoritative in-process copy of GPU register state, replacing the old `BOUND_SHADERS` global. Lives in `crates/gnm/src/state.rs`.
- **DirtySource** — a trait in `crates/core/src/dirty.rs` that abstracts guest-memory write-watch notifications. The x86jit-backed impl uses `watch_range` to detect guest writes; `AlwaysDirty` is a fallback for tests. Used by `ResourceCache` to invalidate uploaded buffers.
- **ResourceCache** — `crates/gnm/src/cache/mod.rs`; maps guest buffer addresses to backend resource IDs and uploads vertex/index/const buffer data on first use, re-uploading when the backing guest memory is dirtied. Issues fire-and-forget `BackendCmd` messages to the GPU backend.
- **ShaderRef** — enum representing a bound shader before resolution. `ShaderRef::Embedded { stage, id }` names one of the built-in embedded SPIR-V shaders. `ShaderRef::GcnBinary { addr, len, res }` carries the guest address, byte length, and RSRC-decoded register counts of a real `.sb` blob. Lives in `crates/gnm/src/shader/source.rs`.
- **BackendCmd** — fire-and-forget command enum (`crates/core/src/gpu.rs`) sent from the GNM thread to the display/GPU thread over `GpuBackend::run_command_list`. Keeps `ps4-gnm` Vulkan-free; it carries the buffer/texture-upload and pipeline variants (`PipelineId`, `CreatePipeline`, `BindPipeline` — see decision-7).
- **wave64** — GCN's SIMD execution unit: 64 lanes running in lockstep, each lane holding one VGPR value. `EXEC` is a 64-bit mask controlling which lanes are active. The CPU wave interpreter in `crates/gcn` (the correctness oracle) models this as `WaveState { vgprs[64], sgprs, exec, vcc, scc, … }`.
- **V# / T# / S#** — GCN hardware descriptor types encoding buffer/texture/sampler bindings as packed bitfields in SGPRs. V# (buffer descriptor, 128-bit) carries base address, stride, and size for vertex/constant fetch. T# (texture descriptor) and S# (sampler descriptor) carry image layout and filter state. Decoded in `crates/gnm/src/vbuf.rs`.
- **scePad / virtual DualShock** — the controller API; unemups4 maps the host keyboard onto a virtual DualShock via `crates/libs/src/libscepad/`.
- **app0 / system** — the guest's filesystem roots: `game_data/app0` mounts as `/app0` (app data), `game_data/system` as `/system`.
- **KernelInterface / KernelBridge** — the trait (`ps4-core`) and its concrete implementation (`ps4-kernel`) that keep the crate graph acyclic: `libs` calls the trait, `kernel` provides the bridge.
- **goblin** — the Rust ELF-parsing crate used by `ps4-loader`.

## Backlog.md / workflow

- **task-N** — a Backlog.md task id (`task-1`, `task-2`, …; prefix set by `task_prefix` in `backlog/config.yml`). The canonical short reference for work items in this repo.
- **status** — a Backlog.md task's column: `To Do`, `In Progress`, or `Done`. See [`working-on-tasks.md`](working-on-tasks.md).
- **draft** — a task parked in `backlog/drafts/` (created with `--draft`); not yet on the board.
- **board** — the kanban view (`backlog board`); `--plain` gives the AI-/grep-friendly listing.
- **decision** — an architecture/technology record in `backlog/decisions/` (`backlog decision create`). See [`decisions.md`](decisions.md).
