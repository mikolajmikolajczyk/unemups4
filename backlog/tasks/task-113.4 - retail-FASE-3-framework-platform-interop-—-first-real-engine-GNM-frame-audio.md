---
id: TASK-113.4
title: >-
  retail FASE 3: framework + platform interop — first real-engine GNM frame +
  audio
status: Done
assignee: []
created_date: '2026-07-14 08:28'
updated_date: '2026-07-23 18:41'
labels:
  - retail
  - gpu
  - hle
dependencies:
  - TASK-113.3
parent_task_id: TASK-113
priority: medium
ordinal: 116000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
FASE 3 (parent epic). Once managed code runs (FASE 2), the managed framework drives the native platform layer: video-out init, graphics-device creation (-> GNM command buffers), audio-device init (-> native audio middleware), input, timing. METHOD: same triage loop, now hitting graphics/audio/input APIs. Wire the real engine's GNM submits into the existing GNM -> SPIR-V -> Vulkan path; real (non-synthetic) shaders will surface recompiler coverage gaps -> fix. Route the audio middleware to the sceAudioOut sink (or HLE its mixer). Keep recompiled SPIR-V MoltenVK/Metal-portable. Granular gaps filed pull-driven. Boundary: no crypto; assets local + gitignored.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 the engine's video-out + graphics device init succeed and a GNM command stream reaches our backend
- [ ] #2 the first real-engine GNM frame renders to the window (PNG-by-eye oracle)
- [ ] #3 the audio middleware initializes and produces first output through our sink
- [ ] #4 real-shader recompiler gaps surfaced here are filed as follow-up tasks
<!-- AC:END -->

## Notes

### 2026-07-15 — audio bring-up (Celeste, CUSA11302) — smoke-loop pass

Drove the smoke loop through the audio walls. All changes uncommitted (for review).

Stubs added:
- `crates/libs/src/libscaudioout/mod.rs`
  - `sceAudioOutSetVolume` [NID b+uAV89IlxE] (SCE_AUDIO_OUT_SET_VOLUME) — accept + ignore per-channel volume (single host stream at unity), `vol` ptr guarded with `is_guest_ptr`, return 0.
  - `sceAudioOutOutputs` [NID w3PdaSTSwGE] (SCE_AUDIO_OUT_OUTPUTS) — batch variant; iterates the guest `{handle,ptr}` array and forwards each to the existing `sceAudioOutOutput` path (same cpal sink + pacing). `states` ptr guarded.
- `crates/libs/src/libscngs2/mod.rs` (new module; registered `pub mod libscngs2;` in lib.rs, `LIB_SCE_NGS2` in libs.rs) — minimal Ngs2 stubs, all out-params `is_guest_ptr`-guarded:
  - `sceNgs2SystemCreateWithAllocator` [mPYgU4oYpuY] → non-null opaque system handle
  - `sceNgs2RackCreateWithAllocator` [U546k6orxQo] → non-null opaque rack handle
  - `sceNgs2RackGetVoiceHandle` [MwmHz8pAdAo] → non-null opaque voice handle
  - `sceNgs2VoiceControl` [uu94irFOGpA] → 0
  - `sceNgs2VoiceGetStateFlags` [rEh728kXk3w] → writes flags=0 (idle)
  - `sceNgs2SystemRender` [i0VnXM-C9fc] → 0 (no mixer; buffers left guest-zeroed = silence — real PCM still flows via sceAudioOut)
  - `sceNgs2ParseWaveformData` [hyVLT2VlOYk] → 0

Wall progression:
1. FATAL `sceAudioOutSetVolume` → stubbed → next
2. FATAL `sceNgs2SystemCreateWithAllocator` → added libscngs2 (7 imported Ngs2 syms) → next
3. Audio subsystem fully up: SoundSystem init → sceAudioOutInit ×2 → sceAudioOutOpen (handle=1, grain=256, rate=48000, fmt=5) → sceAudioOutSetVolume (flag=0xff, vol[0]=32768 unity) → Ngs2 system + 3 racks + 2 voices created → scePthreadCreate "AudioOutThread" → **host cpal output stream opened (48000 Hz stereo F32)**. Guest then advances past audio into sceUserServiceInitialize.

FINAL wall (STOP): FATAL missing symbol `sceImeKeyboardOpen` [NID eaFXjfJv3xs] — the IME (on-screen keyboard / **input**) subsystem, hit right after `sceUserServiceInitialize`. This is a **non-audio subsystem** → stop-condition #1 (and #3: all audio-out/init walls cleared, guest advanced into UserService/IME init). Different milestone (input).

Not hit this round (so not stubbed, per scope): `sceAvPlayerGetAudioData`, `sceNpTrophyCreateHandle`, `sceNpTrophyDestroyHandle`, other stubbed-missing sceAudioOut*/Ngs2* — guest hasn't called them yet.

AC #3 (audio middleware initializes + produces first output through our sink): substantially met at the init level — Ngs2 + AudioOut init succeed and the cpal host stream opens; whether audible PCM is submitted depends on the guest getting past the IME wall into its render/audio loop.

Green: `cargo fmt -p ps4-libs` (reordered lib.rs mod decl to alphabetical), `cargo clippy -p ps4-libs --all-targets --all-features -D warnings` clean, `cargo test -p ps4-libs` 15 passed. Release binary rebuilt.

Next wall for whoever picks up input: `sceImeKeyboardOpen` [eaFXjfJv3xs] (libSceIme, input milestone).

---

### Session 2026-07-15 — platform-init walls (IME → mouse → user-event → savedata init)

Drove the smoke loop through the input / platform-service init walls that follow audio. All minimal offline-platform stubs (return success / typed default), guarded with `is_guest_ptr` where they take guest pointers. Build recipe: `touch app/unemups4/src/main.rs` + `nix develop cargo build --release -p unemups4`; run on host `LD_LIBRARY_PATH=/usr/lib UNEMUPS4_BACKEND=interp`.

Stubs added:
- **libsceime** (new module, `LIB_SCE_IME`): `sceImeKeyboardOpen` [eaFXjfJv3xs] `(userId, *param)` → 0. On-screen keyboard; no host equivalent.
- **libscemouse** (new module, `LIB_SCE_MOUSE`): `sceMouseInit` [Qs0wWulgl7U] `()` → 0. Optional USB-mouse subsystem; no device attached.
- **libsceuserservice** (extended): `sceUserServiceGetEvent` [yH17Q6NWtVg] `(*event)` → `SCE_USER_SERVICE_ERROR_NO_EVENT` (0x80960009). Login/logout event poll; one always-logged-in player, queue always empty → guest drains and moves on.
- **libscesavedata** (new module, `LIB_SCE_SAVE_DATA`): `sceSaveDataInitialize3` [TywrFKCoLGY] `(*initParam)` → 0. Subsystem init only — no I/O.

Wall progression this session:
1. `sceImeKeyboardOpen` → libsceime stub → next
2. `sceMouseInit` → libscemouse stub → next
3. `sceUserServiceGetEvent` → GET_EVENT returns NO_EVENT → next
4. `sceSaveDataInitialize3` → libscesavedata init stub → next
5. `sceSaveDataMount2` [0z45PIH+SNI] → **STOP**

FINAL wall (STOP — condition #4, design decision): FATAL missing symbol `sceSaveDataMount2` [NID 0z45PIH+SNI]. This is real save-data I/O, not a boot-init no-op: `sceSaveDataMount2(*mount, *result)` allocates/opens a save slot and writes back a mount-point path (`/savedataN`) + required-block info the game then reads/writes files through. The emulator already has a real host FS mount layer (`crates/kernel/src/fs.rs` — `mount(guest_path, host_path)`, prefix-union resolution). Wiring mount2 means deciding: the host savedata dir (must be local + gitignored under the game path, per rules), the guest mount-point string, how to fill the mount-result out-param, and whether to report an existing save vs a fresh one. That's a design decision → stopped here rather than fake it. Log tail: guest calls `sceSaveDataInitialize3` (ok) then immediately `sceSaveDataMount2` and FATALs.

Everything before it clears cleanly; the guest is not idling — it's actively marching through boot init and stops the instant it needs a real save mount.

Green: `cargo fmt -p ps4-libs`, `cargo clippy --release -p ps4-libs --all-targets --all-features -D warnings` clean, `cargo test -p ps4-libs` 15 passed. Prior uncommitted audio stubs left intact. Nothing committed.

---

### Session 2026-07-15 — real local persistent save-data mount (`sceSaveDataMount2`)

Cleared the `sceSaveDataMount2` wall with a REAL local persistent mount (not a fake). The guest's `System::SaveData::Mount`/`GetMountPoint`/`Unmount` (demangled from the dlsym probes) drive the standard SCE savedata ABI; mount now backs the slot with a real host dir and registers a guest mount point so later `sceKernelOpen("/savedataN/...")` resolves to it via the FS union mount.

**Struct offsets — reverse-engineered from the guest binary at runtime** (temporary hex-dump stub on `sceSaveDataMount2`, then removed):
- Request (`SceSaveDataMount2`): `userId` u32 @ +0x00; `dirName` **pointer** @ +0x08 → a 32-byte NUL-terminated `SceSaveDataDirName` (observed `"SAVEDATA00"`, so NOT inline as first guessed); `blocks` u64 @ +0x10 (observed 300); `mountMode` u32 @ +0x18 (observed 1). Confirmed by dumping 96 req bytes + following the +0x08 pointer (`req+08 = 0x40065bdf4`, target ascii = `SAVEDATA00`).
- Result (`SceSaveDataMountResult`): `mountPoint` 16-byte char array @ +0x00; `requiredBlocks` u64 @ +0x10; `mountStatus` u32 @ +0x1c. Semantics confirmed by the demangled `_ZNK6System8SaveData13GetMountPointEv` (returns the 16-byte mount point the game reads back) and the standard SCE result layout; verified end-to-end because the guest read `/savedata0` back via GetMountPoint and then cleanly unmounted it (a probe cycle).

**Mount design (maintainer-decided):**
- Host save root: `<title_dir>/savedata/<dirName>/` — i.e. `/home/mikolaj/PS4/CUSA11302/savedata/SAVEDATA00/`. Anchored on `host_path("/app0/eboot.bin").parent()` so it resolves to the DUMP dir (not the dev `game_data/app0` union layer), lives outside the repo, and is never committed. Created on mount if missing (trusted-homebrew: the observed mount uses mode=1 without the CREATE bit yet expects the slot to exist).
- Guest mount point: `/savedata{N}` assigned per distinct dirName, registered via `FileSystem::mount`. `mountStatus` = 1 (freshly created) vs 0 (re-mounted existing). `requiredBlocks` = request `blocks` (or 4096 default).
- New `KernelInterface` methods (mirroring `load_start_module`/`module_dlsym` plumbing): `savedata_mount(user_id, dir_name, blocks, mount_mode) -> (mount_point, status, blocks)`, `savedata_umount(mount_point)`, `savedata_dir_count()` — trait in `crates/core/src/kernel.rs`, impl on `Process` in `crates/kernel/src/process.rs`, delegated by `KernelBridge` in `crates/kernel/src/bridge.rs`. Added `FileSystem::unmount` (inverse of `mount`).
- Handlers in `crates/libs/src/libscesavedata/mod.rs`: real `sceSaveDataMount2`, plus `sceSaveDataUmount2` (v2, request→mountPoint ptr) and `sceSaveDataUmount` (v1, mountPoint struct directly). All guest pointers `is_guest_ptr`-guarded.

**Wall progression this session:**
1. `sceSaveDataMount2` → mounted `SAVEDATA00` → `/savedata0`, GetMountPoint read it back → `sceSaveDataUmount` (probe cycle, clean) → next
2. `sceKernelVirtualQuery` [NID rVjRvHJ0X6c] → **STOP**

**FINAL wall (STOP — non-savedata wall):** FATAL missing symbol `sceKernelVirtualQuery` [NID rVjRvHJ0X6c]. This is a kernel memory-region query (Mono GC / JIT probing its address space), NOT save-data — a separate subsystem. The savedata bring-up for this boot phase is complete: the game did a mount→GetMountPoint→umount existence probe and moved on to memory introspection; real save file I/O comes later. Log tail: `sceSaveDataInitialize3` → `sceSaveDataMount2` (ok, `/savedata0`, status=1) → `sceSaveDataUmount` (ok) → FATAL `sceKernelVirtualQuery`.

**Persistence VERIFIED:** run 1 creates `/home/mikolaj/PS4/CUSA11302/savedata/SAVEDATA00/` and reports `mountStatus=1` (created); run 2 re-mounts the same dir and reports `mountStatus=0` (existing) — the dir survives across runs.

**No regression:** `examples/ps4-fs` (exercises the FS layer I touched) still passes all its tests.

Green: `cargo fmt`, `cargo clippy -p ps4-libs -p ps4-kernel -p ps4-core --all-targets --all-features -D warnings` clean, `cargo test -p ps4-libs -p ps4-kernel -p ps4-core` all pass. Temporary hex-dump RE stub removed. Nothing committed.

---

### Session 2026-07-15 — `sceKernelVirtualQuery` (REAL) + `sceCommonDialog*` → reached the GNM graphics boundary

Drove the smoke loop from the `sceKernelVirtualQuery` wall through platform init to the FIRST GNM (graphics) call — the FASE-3 milestone boundary. STOPPED there (did not start implementing GNM).

**Wall progression this session:**
1. `sceKernelVirtualQuery` [NID rVjRvHJ0X6c] → implemented REAL (fills `*info` from the tracked VMA) → next
2. `sceCommonDialogInitialize` [NID uoUpLGNkygk] → stubbed success (+ `sceCommonDialogIsUsed`) → next
3. Guest ran `Graphics::GraphicsSystem::Initialize` → `sceVideoOutOpen` (executed OK) → **STOP**: FATAL missing symbol `sceGnmAddEqEvent` [NID b0xyllnVY-I] — the FIRST GNM/graphics-submit wall (milestone boundary per the task's stop condition).

**`sceKernelVirtualQuery` — REAL (we had the data).**
- `int sceKernelVirtualQuery(const void* addr, int flags, SceKernelVirtualQueryInfo* info, size_t infoSize)`. Registered under `SyscallId::SCE_KERNEL_VIRTUAL_QUERY` (95087) in `crates/libs/src/libkernel/mman.rs`.
- Plumbing mirrors `savedata_mount`: new `KernelInterface::virtual_query(addr, find_next) -> Option<VqRegion>` (trait + `VqRegion` struct in `crates/core/src/kernel.rs`), impl on `Process` in `crates/kernel/src/process.rs`, delegated by `KernelBridge` in `crates/kernel/src/bridge.rs`. The lookup itself is a new `VirtualMemoryManager::query_region(addr, find_next) -> Option<MemoryVma>` (default `None`; overridden by `VmMemoryManager` in `crates/memory/src/vm_backend.rs` using the existing `containing_vma` + a BTreeMap `range(addr..)` for the FIND_NEXT bit).
- **Struct offsets — `SceKernelVirtualQueryInfo` (Orbis ABI, total 0x48 bytes):** start(void\*)@0x00, end(void\*)@0x08, offset(off_t)@0x10, protection(int)@0x18, memoryType(int)@0x1C, packed bit-flags(u8)@0x20 (LSB-first: isFlexible/isDirect/isStack/isPooled/isCommitted), name(char[32])@0x21. **Confirmed against the binary:** a temporary `info!` arg-log showed the guest passing `infoSize=0x48` exactly (= our `VQ_INFO_SIZE`), and after filling the struct the guest ACCEPTED the values and advanced (Mono GC continued past the query into common-dialog / graphics init) — a wrong layout would have faulted or looped. Honors `infoSize` (never writes past it); `info` pointer is `is_guest_ptr`-guarded. Returns `-EINVAL` when `addr` is in no region. Observed call: `addr=0x40ec08000, flags=0x0, infoSize=0x48` → resolved to a tracked region, returned 0. Arg-log downgraded to `debug!` (GC calls it very frequently) — kept, not removed, as it is standard `[SYSCALL]` diagnostics parity with the other mman handlers; no temporary stubs remain.
- `protection` reports the VMA's PS4 prot bits (R=1/W=2/X=4, which match our `MemoryProtection`); `memoryType`=0 (we don't distinguish onion/garlic); `isFlexible` set for `dynamic_alloc`/flexible-named runtime maps, `isDirect` otherwise; `isCommitted` always set for a live region.

**`sceCommonDialog*` — stubbed (no host equivalent).** New `crates/libs/src/libscecommondialog/mod.rs` + `LIB_SCE_COMMON_DIALOG` const: `sceCommonDialogInitialize` → 0, `sceCommonDialogIsUsed` → 0 (no dialog in use). There is no system-dialog surface on a native host; the title just needs init to succeed. IDs `SCE_COMMON_DIALOG_INITIALIZE` (92233) / `SCE_COMMON_DIALOG_IS_USED` (92234) both pre-exist in generated_syscalls.

**FINAL wall (STOP — graphics boundary):** FATAL missing symbol `sceGnmAddEqEvent` [NID b0xyllnVY-I], reached right after `sceVideoOutOpen` executed. This is the FASE-3 milestone boundary: the managed framework's `GraphicsSystem::Initialize` opened video-out and is now setting up the GNM command/EQ-event path (AC#1 territory). Per the task's stop condition, did NOT begin implementing GNM. Note: the loader already link-stubs the whole `sceGnm*` family (`0xc00000..` reporting stubs), so the first *called* one (`sceGnmAddEqEvent`) is where it stops; implementing the GNM submit path is the next chunk of work (AC#1/#2).
Log tail (noise filtered): `sceCommonDialogInitialize` → dlsym probes for `Graphics::GraphicsSystem::*` → `sceVideoOutOpen` → FATAL `sceGnmAddEqEvent`.

**No regression:** `examples/ps4-fs` runs clean (no FATAL/panic). Build green (`cargo build --release -p unemups4`, 0 errors). `cargo fmt`, `cargo clippy -p ps4-libs -p ps4-kernel -p ps4-core -p ps4-memory --all-features -D warnings` clean (a pre-existing `needless_range_loop` in `crates/gpu/src/bin/diff_harness.rs` — untouched by this work — is the only workspace-wide clippy error), `cargo test -p ps4-libs -p ps4-kernel -p ps4-core -p ps4-memory` all pass (53 tests). Nothing committed; all work left uncommitted on `feat/retail-fase3-platform-init`.
