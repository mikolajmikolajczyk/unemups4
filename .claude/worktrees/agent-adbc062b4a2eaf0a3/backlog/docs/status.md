# Status

Snapshot of what works, what's in flight, what's broken. **Not the roadmap** — roadmap lives in Backlog.md tasks (`backlog task list --plain`).

Update this when a feature lands, breaks, or gets pulled. Stale status is worse than no status — if you can't keep it fresh, link straight to `backlog task list` filters instead.

## Works

- Loads and runs plain (unencrypted) PIE x86-64 ELF homebrew. Input is a
  user-supplied already-decrypted file: a plain ELF, or a decrypted-but-still-
  SELF-wrapped executable — the loader (`ps4_loader::self_container`) auto-detects
  the magic and extracts the inner ELF. It performs no decryption; a still-
  encrypted or compressed SELF is rejected with an explicit error.
- Resolves library imports (`sceKernel*`, `scePad*`, …) to Rust handlers — ~90 of them.
- Threads, TLS, mutex/cond/rwlock (rwlock is currently an exclusive mutex).
- Guest code runs on the **x86jit** engine (interpreter + Cranelift JIT, identity-mapped
  arena); the native execution backend is removed (tasks 3–9 done).
- Presents software-rendered output through Vulkan; keyboard mapped to a virtual DualShock via `scePad`.
- Generates syscall id/NID/metadata tables at build time.
- GPU phase 3/3.5 done: PM4 Type-3 trace decoder, `libSceGnmDriver` stubs, embedded-shader
  draw pipeline — a single GPU frame can be produced with pre-built embedded SPIR-V shaders.
- GPU phase 4 spine landed (tasks 36/37/43/44/48/49 done):
  - OrbShdr `.sb` parser (`crates/gnm/src/shader/sb.rs`) — parses `ShaderBinaryInfo` header
    and semantic tables from guest shader blobs.
  - Shadow register file `RegFile`/`GpuState` (`crates/gnm/src/state.rs`) — CONTEXT/SH/UCONFIG
    banks, replaces the old `BOUND_SHADERS` global.
  - `DirtySource` trait + x86jit-backed impl (`crates/core/src/dirty.rs`) — watch-range-driven
    guest-memory invalidation seam.
  - `ResourceCache` (`crates/gnm/src/cache/mod.rs`) — guest-side id model; vertex/index/const
    buffer upload-on-use with dirty invalidation.
  - Register-based shader binds — `SPI_SHADER_PGM_LO/HI` → `ShaderRef::GcnBinary`; HLE
    `sceGnmSet*Shader` emits real PM4 (`crates/gnm/src/exec.rs`, `state.rs`).
  - Synthetic GCN shader corpus (`crates/gcn/tests/corpus/`) — assembled `.s` sources rendered
    to OrbShdr blobs + test harness (`ps4-gcn` crate).

## In flight

See `backlog task list -s "In Progress" --plain`.

## Broken / regressions

Not-yet-implemented (by design, not regressions):

- No SELF/fSELF decryption and no `DT_SCE_*` relocation/NID tables — an *already-decrypted* SELF has its inner ELF extracted and loaded via `goblin`, but a still-encrypted retail binary is rejected ("must be decrypted first"), never decrypted.
- No real GCN GPU draw: the phase-4 spine (`.sb` parser, shadow registers, `ResourceCache`) is
  landed, but the GCN ISA decoder (task-38), wave CPU interpreter oracle (task-39),
  GCN→SPIR-V recompiler (task-40), and real-shader draw keystone (task-53) are all still
  To Do — guests issuing real GNM draw calls with GCN shaders show nothing.
- Guest memory reads from parse_sb are unbounded (task-65, fault-safety blocker for task-53).
- Only some `R_X86_64_*` relocation types applied; TLS offsets and multi-prx linking reduced to the single-module case.
- Output fixed at 1920x1080 RGBA8; no swapchain recreation on resize.
- Several higher-level calls (userService, parts of videoOut, signals) just return success.

## Not started

See `backlog task list --plain` (or `backlog board`) filtered by label/milestone. Don't duplicate the task list here.
