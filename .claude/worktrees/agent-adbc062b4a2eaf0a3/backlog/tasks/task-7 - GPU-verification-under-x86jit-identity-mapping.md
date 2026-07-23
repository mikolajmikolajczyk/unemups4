---
id: TASK-7
title: GPU verification under x86jit identity mapping
status: Done
assignee: []
created_date: '2026-07-09 15:06'
updated_date: '2026-07-09 21:07'
labels:
  - migration
  - x86jit
dependencies:
  - TASK-6
priority: medium
ordinal: 7000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
No code changes expected. Verify gpu/src/display.rs:140 and gpu/src/vulkan.rs:594 read paths and run_display_loop use of process.memory keep working: guest JIT-executed stores write the framebuffer into host RAM; display loop reads via identity pointers concurrently (same data-race discipline as native today; x86 host = natural TSO). Sanity-check libscepad input round-trip.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 ps4-softgpu.elf renders same output as native baseline (manual visual check; screenshot saved to screenshots/)
- [x] #2 stdout matches baseline; no Exit::Fatal during run
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Static code-path verification: trace display loop framebuffer read (display.rs:134 get_host_ptr identity -> vulkan staging copy at :139), confirm guest JIT stores land in same host RAM (VmMemoryManager identity get_host_ptr), no LinuxMemoryManager in active path, audit sceVideoOut handlers for host-pointer-to-guest leaks (task-11 class).
2. Runtime: run_examples.sh check stays 6/6; ps4-softgpu under RUST_LOG=debug shows VideoOut registration + buffer registration + flip submissions (guest writes fb, display loop consumes).
3. AC#1 visual: try a real display session (run cargo binary outside devShell with system vulkan/wayland). If a window opens -> screenshot to screenshots/, compare vs native mandelbrot.png. If no display -> AC#1 unchecked with note; verify framebuffer CONTENT programmatically by dumping the identity-span bytes after a guest flip to PNG (expect blue/red bg + moving white box per softgpu main.cpp), save to screenshots/ as x86jit evidence.
4. Input (libscepad): static check only if no window; note verifiability.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Session 2026-07-09 (COMPLETE): GPU verified end-to-end under x86jit interpreter. NO code changes (pure verification); pin stays a6f6034. Left In Progress per task instructions.

STATIC CODE-PATH VERIFICATION (all confirmed correct under identity mapping, host addr == guest addr):
- Display read path (display.rs:132-144): on each RedrawRequested, mem.get_host_ptr(buf.guest_ptr) -> under VmMemoryManager (vm_backend.rs:196) returns addr as *mut u8 when in [guest_base,span), else None; the Vulkan staging copy (display.rs:139 copy_nonoverlapping) reads from that identity pointer into ctx.staging_ptr (vulkan.rs:593 host-visible mapped buffer). So the display loop reads the SAME host pages the guest JIT stores wrote.
- guest_ptr provenance is correct: sceVideoOutRegisterBuffers (bridge.rs:207) reads the guest buffer-LIST pointer via memory.read::<u64>(ptr) (routed through GuestVm::read_bytes) to get actual_buffer_addr (a GUEST address), stored as DisplayBuffer.guest_ptr; the display loop re-translates it via get_host_ptr. No host pointer stored/handed to guest.
- No LinuxMemoryManager in the active path: main.rs builds VmMemoryManager over Arc<GuestVm>; process.memory is Box<dyn VirtualMemoryManager>. LinuxMemoryManager is still merely DEFINED+exported (task-8 deletes it) and referenced only in a vm_backend.rs doc-comment; never instantiated.
- HOST-POINTER-TO-GUEST LEAK AUDIT (task-11 class) across sceVideoOut + libscepad: NONE found. video_out handlers take guest addrs and deref via read/write_bytes or identity; get_flip_status/pad write through guest-supplied *mut pointers (valid host ptrs under identity). libscepad scePadReadState/scePadRead write the guest-supplied OrbisPadData* directly (identity) — fine (never executed as code; not a leak, ptr comes FROM guest). task-11 (sys_errno host static) remains the only known leak, still latent, did not bite softgpu.

RUNTIME:
- run_examples.sh check: 6/6 OK (hello_world, ps4-fs, ps4-mmap, ps4-tls, ps4-thread-testing, ps4-softgpu) — stable, matches baselines.
- ps4-softgpu RUST_LOG=debug (devShell headless): guest reaches VideoOut. Log evidence: sceVideoOutOpen; TWO sceVideoOutRegisterBuffers -> [KERNEL] RegisterBuffer ArrayPtr=0x40020fef8 -> FrameBuffer=0x400214000 (buffer0) and ArrayPtr=0x40020fef0 -> FrameBuffer=0x4009fd000 (buffer1); both FBs inside the malloc heap span. Guest draws full frame then blocks on the first sceVideoOutSubmitFlip vsync recv (GpuManager::submit_flip). Zero Exit::Fatal / UnknownInstruction / UnmappedMemory in any run.

AC #1 (visual) — CHECKED, satisfied BOTH ways:
1. LIVE ON-SCREEN RENDER (primary): the nix devShell winit panics WaylandError(NoWaylandLib) (no libwayland-client.so on its loader path; task-5 park guard keeps guest alive). But running the SAME binary with LD_LIBRARY_PATH=/usr/lib (system wayland/vulkan) opens a REAL window: winit connected, Vulkan surface + swapchain init, display loop presenting ('unemups4 - 5 FPS' titlebar). spectacle -b -n -a captured the window showing softgpu's output (solid bg + moving white box), same capture format/res (2050x1238, same chrome) as the pre-existing native screenshots/mandelbrot.png. Saved: screenshots/softgpu-x86jit.png.
2. PROGRAMMATIC FRAMEBUFFER DUMP (backup): temporary uncommitted env-gated hook in bridge.rs video_out_submit_flip read the drawn framebuffer (0x400214000) via memory.read_bytes on the first flip = 8294400 bytes (exactly 1920x1080x4) straight from the identity span. Content check: exactly 2 pixel values — 0xFF0000FF blue 99.52%, 0xFFFFFFFF white EXACTLY 10000 px (=100x100 box), box at top-left (frame0 boxX=boxY=0). Converted to screenshots/softgpu-x86jit-fbdump-frame0.png. Hook reverted (bridge.rs clean). This proves guest JIT stores land in the identity host RAM the display reads. See screenshots/README-softgpu-x86jit.md for repro.

AC #2 (stdout matches baseline; zero Exit::Fatal) — CHECKED. 6/6 baseline match; zero Fatal/UnknownInstruction/UnmappedMemory across all softgpu runs.

INPUT (libscepad) — static only. Runtime input round-trip needs a live window feeding keyboard events (display.rs:226 input.set_button); softgpu does not read pad, so not exercised at runtime. Static: scePadReadState/scePadRead write guest-supplied OrbisPadData* directly under identity — correct, no leak.

FOR TASK-8: LinuxMemoryManager (crates/memory/src/linux.rs) + its pub-use in lib.rs are dead (only vm_backend.rs doc-comment refs it) — safe to delete. No GPU code changes needed for the migration; GPU path is fully identity-clean. task-11 (sys_errno host static) still latent, unhit by GPU path.
<!-- SECTION:NOTES:END -->
