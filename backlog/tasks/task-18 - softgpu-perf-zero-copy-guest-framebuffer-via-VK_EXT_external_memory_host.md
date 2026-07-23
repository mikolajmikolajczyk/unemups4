---
id: TASK-18
title: 'softgpu perf: zero-copy guest framebuffer via VK_EXT_external_memory_host'
status: Done
assignee: []
created_date: '2026-07-10 09:29'
updated_date: '2026-07-10 18:15'
labels:
  - perf
  - gpu
dependencies:
  - TASK-17
priority: low
ordinal: 18000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every frame copies 8.3MB from the guest framebuffer to a HOST_VISIBLE|HOST_COHERENT staging buffer (display.rs:131-146, vulkan.rs:585-594) before the buffer->image transfer. Under identity mapping the guest framebuffer has a stable host pointer, so VK_EXT_external_memory_host can import those pages directly as a VkBuffer and skip the memcpy entirely. Secondary win after task-16/task-17 (~1-2ms/frame). Design notes: import requires minImportedHostPointerAlignment-aligned pointer+size (guest buffers are malloc'd — may need alignment handling or per-buffer import at RegisterBuffer time); extension is widely supported on desktop Vulkan but gate on availability and keep the staging-copy path as fallback; guest writes must be visible before the GPU transfer (identity-mapped memory is host-cached — needs correct memory domain handling/HOST_COHERENT import semantics check).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Guest framebuffers imported via VK_EXT_external_memory_host at RegisterBuffer time when the extension is available; per-frame staging memcpy eliminated on that path
- [x] #2 Fallback to existing staging-copy path when extension/alignment unavailable; both paths tested
- [x] #3 softgpu visual output identical; fps delta recorded in task notes
<!-- AC:END -->



## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
CONCRETE PLAN (agent session 2026-07-10, IMPLEMENTED). Follows the handoff design map below; ash 0.38 API verified against the vendored crate source.

vulkan.rs:
- VulkanContext gains `ext_mem_host: Option<ash::ext::external_memory_host::Device>` and `min_import_alignment: u64`.
- create_device: enumerate_device_extension_properties(pdevice), scan for `ash::ext::external_memory_host::NAME`. Enable it (+ its dep VK_KHR_external_memory, both enabled if present) alongside VK_KHR_swapchain when available AND `UNEMUPS4_NO_EXTMEMHOST` is unset. Returns `ext_enabled: bool`.
- new(): if ext_enabled, build the wrapper `external_memory_host::Device::new(&instance,&device)` and query minImportedHostPointerAlignment via PhysicalDeviceExternalMemoryHostPropertiesEXT chained into get_physical_device_properties2. INFO-log the selected path ("zero-copy import available, alignment=N" vs "staging-copy path (VK_EXT_external_memory_host unavailable/disabled)").
- New helper `VulkanContext::try_import_host_buffer(host_ptr, size) -> Option<ImportedBuf>`: gates on alignment, calls the raw `get_memory_host_pointer_properties_ext` fp, creates a TRANSFER_SRC VkBuffer with ExternalMemoryBufferCreateInfo{HOST_ALLOCATION_EXT}, allocates VkDeviceMemory with ImportMemoryHostPointerInfoEXT, binds. Any vk error -> None (fall back). All unsafe blocks documented.

display.rs:
- Import cache `HashMap<(i32,u32), ImportedBuf>` alongside `buffers`. On RegisterBuffer, try import lazily (once per key); re-register at a new ptr destroys+frees the stale import first.
- Flip path: select source buffer. If an imported buf exists for current_target -> use imported.buffer as copy source, SKIP the memcpy. Else memcpy into staging + use ctx.staging_buffer. Thread the chosen source into record_command_buffer(ctx, image_index, src_buffer).
- task-17 signal: on the STAGING path keep the early flip-queued signal exactly as today (after memcpy). On the ZERO-COPY path there is no memcpy to anchor to and the GPU reads guest memory directly, so signalling early would race the guest's next write into the same buffer. Prefer correctness: defer the guest vsync signal until AFTER queue_submit on the zero-copy path (the submit has captured the transfer that reads the imported memory; combined with guest double-buffering the guest may then proceed). Documented inline + in notes.

Env gate: UNEMUPS4_NO_EXTMEMHOST=1 forces the staging path for A/B testing.

STOP CRITERION honoured: scope stayed bounded; leak-on-exit convention kept (no Drop), only stale-on-reregister freed.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
=== SESSION 2026-07-10 (implement + verify headless) — takeover from prior design/handoff session ===

IMPLEMENTED per the design map below. Code landed on branch feat/zero-copy-fb. The prior session's "NOT STARTED in code" note (further down) is now historical — code has since been written and verified as far as a headless env allows.

WHAT LANDED:
- vulkan.rs: VulkanContext gains `ext_mem_host: Option<ash::ext::external_memory_host::Device>` + `min_import_alignment: u64`. create_device probes device extensions (enumerate_device_extension_properties), enables VK_EXT_external_memory_host + its dep VK_KHR_external_memory only when both present AND UNEMUPS4_NO_EXTMEMHOST is unset; returns an ext-enabled bool. new() builds the wrapper, queries minImportedHostPointerAlignment via PhysicalDeviceExternalMemoryHostPropertiesEXT chained into get_physical_device_properties2, and INFO-logs the selected present path ("ZERO-COPY ..." vs "STAGING-COPY ...") on startup.
- vulkan.rs: `try_import_host_buffer(host_ptr, size) -> Option<ImportedBuf>` — gates on alignment (is_multiple_of), calls get_memory_host_pointer_properties_ext, creates a TRANSFER_SRC VkBuffer with ExternalMemoryBufferCreateInfo{HOST_ALLOCATION_EXT}, allocates DeviceMemory via ImportMemoryHostPointerInfoEXT, binds. Any vk error → None (fall back). `destroy_imported_buffer` frees stale imports. Every unsafe block documents its invariant.
- vulkan.rs memory-type selection (visibility hardening, spec point 5): PREFER a HOST_VISIBLE|HOST_COHERENT type from (props.memory_type_bits & reqs.memory_type_bits); fall back to any usable type only if none is coherent. Rationale: imported host memory cannot be flushed with vkFlushMappedMemoryRanges, so coherent import is what makes guest writes visible to the GPU transfer without stale-frame artifacts.
- display.rs: per-(hdl,idx) import cache `HashMap<(i32,u32), ImportedBuf>`. Lazy import at RegisterBuffer, ONLY for full-frame (RES_W×RES_H) buffers — the fixed-size GPU copy region would otherwise read past a smaller guest buffer's pages. Re-register at a new host ptr device_wait_idle()s then frees the stale import first. Flip path selects the copy source: imported buffer → skip memcpy; else memcpy into staging as before; the chosen source is threaded into record_command_buffer(ctx, image_index, src_buffer).
- Env gate: UNEMUPS4_NO_EXTMEMHOST=1 forces the staging path for A/B.

TASK-17 SIGNAL DECISION (correctness over speed, per brief):
- STAGING path: keep task-17's early flip-queued signal exactly as before — sent right after the memcpy, which has fully decoupled the guest fb from what the GPU reads.
- ZERO-COPY path: there is NO memcpy to anchor to; the GPU reads the guest's own pages directly during the buffer→image transfer. Signalling early would let the guest overwrite those pages mid-transfer. So the guest vsync signal is DEFERRED to just after queue_submit on this path — the submit has captured the transfer that reads the imported memory, and combined with guest double-buffering the guest may then safely proceed into the OTHER buffer. This is the one subtle point that MUST be validated on real hw (see live A/B below): it is possible zero-copy loses part of task-17's win because the signal moves from after-memcpy to after-submit. Documented inline in display.rs.

HEADLESS VERIFICATION (this session, no Vulkan driver):
- cargo build --release: green.
- cargo clippy --all-targets --all-features -- -D warnings: clean (fixed two inherited lints: manual_is_multiple_of, map_entry).
- cargo test: green (all suites pass; gpu present path is not unit-tested — runtime-only).
- cargo fmt: clean.
- scripts/run_examples.sh check: only divergence is the single expected env line per display example ("ERROR ps4_gpu::display: Failed to initialize Vulkan: Unable to find a Vulkan driver"). No other divergence. The path-selection INFO log does NOT appear headless because VulkanContext::new() fails at driver/instance init before reaching that code — expected.

ACCEPTANCE CRITERIA — all three require a live Vulkan driver to PROVE, so all left UNTICKED (headless env cannot exercise the import/fallback/visual paths). Headless env confirms the code compiles, is clippy/fmt clean, does not regress the oracle, and that the import/fallback/signal LOGIC is present and correctly gated. AC#1/#2/#3 to be ticked by the maintainer after the live A/B below.

=== LIVE A/B PROCEDURE (maintainer, needs a real Vulkan driver) ===
Run the softgpu example under the jit backend, comparing zero-copy vs forced-fallback:

  # A) ZERO-COPY (default when ext available):
  UNEMUPS4_BACKEND=jit cargo run --release -p unemups4 -- examples/ps4-softgpu/ps4-softgpu.elf
  # B) FORCED STAGING FALLBACK (same build):
  UNEMUPS4_NO_EXTMEMHOST=1 UNEMUPS4_BACKEND=jit cargo run --release -p unemups4 -- examples/ps4-softgpu/ps4-softgpu.elf

OBSERVE / RECORD:
1. Startup INFO log: confirm run A prints "Present path: ZERO-COPY (... minImportedHostPointerAlignment=N ...)" and run B prints "Present path: STAGING-COPY (... disabled via UNEMUPS4_NO_EXTMEMHOST ...)". If A prints STAGING-COPY, the device lacks the extension → zero-copy unavailable on this GPU (still correct; AC#1 then N/A on this hw).
2. Import taken vs declined: with the profiler on, watch for the INFO "Zero-copy: imported guest framebuffer at 0x... (N bytes) as VkBuffer" (import succeeded) vs DEBUG "Zero-copy import skipped: host ptr ... not aligned" (guest fb malloc'd unaligned → fell back). Guest FBs are frequently not page-aligned, so a decline here is EXPECTED, not a bug — it just means this fb uses staging.
3. Visual output IDENTICAL between A and B — same colors/geometry, and specifically NO tearing/stale-frame/torn-read artifacts on the zero-copy path (this is the task-17 signal-timing + coherence correctness check). Any flicker/tear on A but not B ⇒ the deferred-signal / coherence handling needs revisiting before ticking AC#3.
4. FPS delta: record steady-state fps for A vs B (the app logs fps; or use the present profiler). Expected win ~1-2ms/frame (secondary to task-16/17). Note whether zero-copy's after-submit signal costs any of task-17's fps gain vs staging's after-memcpy signal.

Tick AC#1 (import path taken, memcpy eliminated) + AC#2 (fallback works, both paths tested via the env gate) + AC#3 (visual identical, fps delta recorded) once 1-4 pass. Leave status In Progress until then.

--- historical (prior handoff session, before code was written) ---


DECISION (STOP per task's explicit 'if it balloons, stop and leave a note' criterion): tasks 19/16/17 are done, verified, committed; 18 attempted next as directed. I stopped BEFORE writing code because the entire runtime path is unverifiable in this environment (headless devShell has NO Vulkan driver -- VulkanContext::new() fails at instance/driver init, so I cannot exercise import, fallback, alignment, or visibility). Committing unexercised unsafe raw-host-pointer GPU import (VkImportMemoryHostPointerInfoEXT) that could corrupt/crash on real hardware is the wrong tradeoff for a LOW stretch task. Handing to the maintainer, who has a Vulkan driver and can implement+verify in one focused session using the map below. No code changed for task-18; branch left clean after task-17.

--- FULL DESIGN + EXACT INSERTION POINTS (ash 0.38, crates/gpu) ---

ash 0.38 exposes the wrapper as ash::ext::external_memory_host::Device (NOT ash::extensions::ext). Construct: ext::external_memory_host::Device::new(&instance, &device); it provides get_memory_host_pointer_properties().

1. DEVICE EXTENSION ENABLE -- vulkan.rs:253-263. Currently device_extensions = [VK_KHR_swapchain]. Before create_device: enumerate_device_extension_properties(pdevice), scan for VK_EXT_external_memory_host (c"VK_EXT_external_memory_host"). If present, push its name ptr into the array and bump enabled_extension_count. Track a bool ext_available.
   - Query minImportedHostPointerAlignment: chain vk::PhysicalDeviceExternalMemoryHostPropertiesEXT into get_physical_device_properties2(pdevice). Typically 4096.

2. VulkanContext NEW FIELDS (struct at vulkan.rs:13-52): add
     pub ext_mem_host: Option<ash::ext::external_memory_host::Device>,
     pub min_import_alignment: u64,
   Populate in new() after device creation. Keep staging_buffer/mem/ptr as the always-present fallback.

3. PER-BUFFER IMPORT CACHE lives in the display loop (display.rs), NOT VulkanContext, because it is keyed by (hdl,idx) known at RegisterBuffer. Add a HashMap<(i32,u32), ImportedBuf> alongside 'buffers'. ImportedBuf { buffer: vk::Buffer, memory: vk::DeviceMemory }. At RegisterBuffer (display.rs:70) OR lazily on first flip for that key:
     a. Resolve host ptr: guest_memory.read().get_host_ptr(guest_ptr). Under identity mapping this is stable.
     b. Gate: only if ext_mem_host.is_some() AND (host_ptr % min_import_alignment == 0). Guest fb is malloc'd -> often NOT page-aligned -> fall back for that buffer (expected, fine).
     c. size = round_up(w*h*4, min_import_alignment).
     d. props = ext.get_memory_host_pointer_properties(EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT, host_ptr as *const c_void). Choose a memory_type_index from props.memory_type_bits.
     e. Create VkBuffer (usage TRANSFER_SRC) with vk::ExternalMemoryBufferCreateInfo{handle_types: HOST_ALLOCATION_BIT_EXT} chained into BufferCreateInfo. get_buffer_memory_requirements.
     f. Allocate VkDeviceMemory with vk::ImportMemoryHostPointerInfoEXT{handle_type: HOST_ALLOCATION_BIT_EXT, p_host_pointer: host_ptr} chained into MemoryAllocateInfo (allocation_size = size, memory_type_index from props masked by reqs.memory_type_bits). bind_buffer_memory.
     g. On ANY vk error at c-f: skip (fall back to staging for this buffer). Cache success only.

4. FLIP PATH -- display.rs record_command_buffer:320-326 (cmd_copy_buffer_to_image src = ctx.staging_buffer) and the memcpy at display.rs:140-144. If imported.get(&key) exists: SKIP the memcpy entirely and pass imported.buffer as the copy source (thread it into record_command_buffer as a param, or select before recording). Else: memcpy to staging + use ctx.staging_buffer as today. Barriers/region unchanged.
   - NOTE the task-17 interaction: the flip-queued signal is currently sent right after the memcpy. On the zero-copy path there is NO memcpy -- move the signal to the equivalent point (right after selecting the source / before record) so semantics stay: guest signalled once the GPU has what it needs. Since zero-copy reads guest memory DIRECTLY during the GPU transfer, the guest MUST NOT overwrite that buffer until the transfer completes -> either (a) rely on guest double-buffering (it draws into the OTHER buffer next) and signal after submit, OR (b) keep signalling early but only safe because guest double-buffers. This is the subtle correctness point to validate on real hw -- zero-copy removes the memcpy decoupling that makes task-17's early signal trivially safe.

5. VISIBILITY: identity-mapped guest memory is host-cached. HOST_ALLOCATION import is HOST_COHERENT per spec for the returned memory types; if a chosen type is non-coherent, a vkFlushMappedMemoryRanges-equivalent is NOT possible on imported host memory -- must pick a coherent-compatible type from props.memory_type_bits. Validate no stale-frame artifacts on real hw.

6. CLEANUP: VulkanContext has NO Drop and leaks all Vulkan resources on exit today (process-exit pattern). Imported buffers/memory can follow the same leak-on-exit convention; if a buffer is re-registered at a new ptr, destroy+free the stale import first to avoid unbounded growth.

7. FALLBACK TEST (AC#2): to exercise the fallback path even where the ext IS available, add an env gate e.g. UNEMUPS4_NO_EXTMEMHOST=1 forcing ext_available=false, so both paths are testable.

VERIFY (maintainer, needs Vulkan driver): ps4-softgpu under UNEMUPS4_BACKEND=jit; (a) confirm import path taken when fb aligned (log it), (b) confirm fallback when forced/unaligned, (c) visual output identical, (d) record fps delta (~1-2ms/frame per task rationale, secondary to 16/17).

2026-07-10 LIVE A/B DONE (maintainer, real Vulkan driver, UNEMUPS4_BACKEND=jit default): (a) zero-copy path taken — "Present path: ZERO-COPY (minImportedHostPointerAlignment=4096)", BOTH guest framebuffers imported ("Zero-copy: imported guest framebuffer at 0x400214000 / 0x4009fd000 (8294400 bytes) as VkBuffer") — mmap'd FBs are page-aligned, real import, no alignment fallback; (b) UNEMUPS4_NO_EXTMEMHOST=1 → "Present path: STAGING-COPY", works; (c) visual output identical, clean, NO tearing / stale frames (validates the deferred after-submit signal + HOST_COHERENT import); (d) fps 60 vs 60 — both vsync-capped, so the memcpy saving is invisible at this load; expected to matter at heavier render. All 3 ACs ticked; merged to main; Done.
<!-- SECTION:NOTES:END -->
