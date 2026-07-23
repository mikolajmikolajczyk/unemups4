# Status

Snapshot of what works, what's in flight, what's not done. **Not the roadmap** — the roadmap lives in Backlog.md tasks (`backlog task list --plain`). The development story is [`doc-7`](<doc-7 - The-unemups4-story-—-a-development-history.md>).

Update this when a feature lands, breaks, or gets pulled. Stale status is worse than no status.

## Works

- **Loader.** Plain PIE x86-64 ELF, and already-decrypted SELF/OELF: the loader auto-detects the magic and extracts the inner ELF, parses the SCE dynamic table (`DT_SCE_*`), resolves NID-hashed imports, and loads multi-`.prx` modules (`sceKernelLoadStartModule`, each `module_start` leaves-first). It performs **no decryption** — a still-encrypted SELF is rejected with an explicit error.
- **CPU.** Guest code runs on the **x86jit** engine (Cranelift JIT + interpreter, identity-mapped arena; interpreter is the correctness oracle). Every guest thread is a host thread with its own `Vcpu` over a shared VM; syscalls trap to Rust handlers, never to the host kernel.
- **Kernel / libs HLE.** ~200 library entry points; threads, TLS, mutex/cond/rwlock (rwlock is currently an exclusive mutex); a filesystem backend with POSIX errno/persistence; a managed-runtime bring-up (Mono AOT + MonoGame + FMOD). The HLE answers as a coherent console state (e.g. the network stack reports "no link").
- **GPU — real GCN path.** `sceGnm*` submits are decoded as PM4 (shadow register file, EOP/EOS sync). The GCN shader ISA is decoded and either **interpreted** (oracle) or **recompiled to SPIR-V** for Vulkan. Working: T#/V#/S# descriptor decode, texture cache, tiling/detile, multi-texture pixel-shader binding, the RECTLIST full-screen-fill primitive. The register-route triangle keystone and the textured-draw milestone are done; real `.sb`-shader draws render on screen. Kept portable toward MoltenVK/Metal.
- **Retail Celeste** (from an already-decrypted dump) boots to **gameplay** with physical gamepad input, correct palette, correct textures. Main menu ~58 fps; in-game ~24–26 fps (peaking ~32).
- **A PS4 Doom port** (the author's own, unpublished) is playable with audio (~62 fps).
- **A second retail title** (Unreal Engine 4) boots through hundreds of imports and renders its first frames before a multi-threaded RHI deadlock.
- **Tooling.** `dcbdump` (`tools/ps4-gnm-scrape/host`) decodes a real-console GPU command capture through the emulator's own PM4 decoder — the ground-truth oracle. Syscall id/NID/metadata tables generated at build time.
- **Provenance.** Every hardware/OS fact is cited to a clean primary source (AMD GCN ISA, Mesa, kernel AMD headers, OpenOrbis SDK/OELF, FreeBSD, the console capture) and pinned with a witness test.

## In flight

See `backlog task list -s "In Progress" --plain`.

## Not done (by design, not regressions)

- **No SELF/fSELF decryption, no keys, no SAMU** — input must be an already-decrypted file.
- **General cross-thread kernel events** (`sceKernelAddUserEvent`/`TriggerUserEvent`/`AddTimerEvent`) are unimplemented — the multi-threaded UE4 RHI deadlock (task-230) is the current far wall.
- **Guest-CPU-bound gameplay** (~24–26 fps): the frame is dominated by interpreted guest code (a write barrier is the hot spot). A faster JIT is the next frontier, not the GPU — but this is a fun/education project, not one chasing native speed.
- Output fixed at 1920×1080 RGBA8; no swapchain recreation on resize.
- Some higher-level calls (parts of userService, videoOut, signals) return success without full behaviour.

## Not started

See `backlog task list --plain` (or `backlog board`) filtered by label/milestone. Don't duplicate the task list here.
