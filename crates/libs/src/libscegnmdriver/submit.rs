//! libSceGnmDriver submit/flip entry points (doc-2 §1). The submit stubs
//! extract the DCB/CCB pointer/size ranges from guest memory and record them into
//! the `GnmDriver` (in `ps4-gnm`), which retains them for the PM4 trace decoder.
//! No PM4 is decoded or executed here — everything is log-and-return-
//! success. Guest ptrs are read through the bounded read seam
//! ([`ps4_core::bounded_read`]), which validates the whole range against the live
//! VMA set (never over-reads).

use crate::context::NativeContext;
use ps4_gnm::driver::driver;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;

/// Monotonic frame counter for `UNEMUPS4_DUMP_SUBMIT` (task-157). Incremented once
/// per `record_submit` batch so our dumps line up frame-for-frame with the real-PS4
/// GNM scraper corpus (data/celeste-real-dcb/) which numbers by flip.
static DUMP_FRAME: AtomicU64 = AtomicU64::new(0);

/// Env-gated (`UNEMUPS4_DUMP_SUBMIT=<dir>`) raw dump of a submitted DCB (or CCB) in
/// the EXACT byte layout the real-PS4 scraper writes, so the same `decode` tool runs
/// on both. Writes `<dir>/ourframeNNNNNN_subN_<flip|nonflip>_<dcb|ccb>.bin`. Off by
/// default (empty/unset env). Reads the guest command buffer through the bounded
/// seam; a rejected range is skipped.
fn dump_submit(frame: u64, sub: usize, ptr: u64, size: u32, flip: bool, is_ccb: bool) {
    let Ok(dir) = std::env::var("UNEMUPS4_DUMP_SUBMIT") else {
        return;
    };
    if dir.is_empty() || ptr == 0 || size == 0 {
        return;
    }
    let Some(src) = ps4_core::bounded_read::bounded_read() else {
        return;
    };
    let Ok(bytes) = src.read_ranged(ptr, size as usize) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let kind = if is_ccb { "ccb" } else { "dcb" };
    let tag = if flip { "flip" } else { "nonflip" };
    let path = format!("{dir}/ourframe{frame:06}_sub{sub}_{tag}_{kind}.bin");
    if let Err(e) = std::fs::write(&path, &bytes) {
        info!("[GNM] UNEMUPS4_DUMP_SUBMIT write {path} failed: {e}");
    }
}

/// Read `count` 64-bit command-buffer GPU addresses from a guest array pointer.
/// The Gnm submit ABI passes the DCB/CCB address arrays as `void*[]` (64-bit
/// GPU VAs); an identity-mapped OpenOrbis `malloc` pointer lives above 4 GB, so
/// reading them as 32-bit truncated the buffer to all-zeros. Read through the
/// bounded seam ([`ps4_core::bounded_read`]); a missing seam (headless) or a
/// rejected range yields an empty Vec (`record_submit` degrades cleanly).
fn read_u64_array(ptr: u64, count: u32) -> Vec<u64> {
    let Some(src) = ps4_core::bounded_read::bounded_read() else {
        return Vec::new();
    };
    let Ok(bytes) = src.read_ranged(ptr, count as usize * size_of::<u64>()) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        .collect()
}

/// Read `count` u32 sizes from a guest array pointer. The size arrays are
/// `uint32_t*` on the Gnm ABI (byte counts), read here via the bounded seam.
fn read_u32_array(ptr: u64, count: u32) -> Vec<u32> {
    let Some(src) = ps4_core::bounded_read::bounded_read() else {
        return Vec::new();
    };
    let Ok(bytes) = src.read_ranged(ptr, count as usize * size_of::<u32>()) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// `sceGnmSubmitCommandBuffers(count, dcb_addrs[], dcb_sizes[], ccb_addrs[], ccb_sizes[])`.
/// Records each DCB/CCB pair for the future PM4 decoder; returns success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SUBMIT_COMMAND_BUFFERS,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSubmitCommandBuffers"
)]
pub fn sce_gnm_submit_command_buffers(
    count: u32,
    dcb_addrs: u64,
    dcb_sizes: u64,
    ccb_addrs: u64,
    ccb_sizes: u64,
) -> i32 {
    record_submit(
        count, dcb_addrs, dcb_sizes, ccb_addrs, ccb_sizes, false, 0, 0,
    );
    0
}

/// `sceGnmSubmitAndFlipCommandBuffers(count, dcb_addrs[], dcb_sizes[], ccb_addrs[],
/// ccb_sizes[], vo_handle, buf_idx, flip_mode, flip_arg)`. Records the DCB/CCB pairs
/// marked as flip-carrying, along with the videoout handle and scanout buffer index
/// the flip targets; returns success.
///
/// ABI: the first 6 args are register-passed (count=arg0(rdi) .. vo_handle=arg5(r9),
/// arg3 read from r10 due to the SYSCALL RCX-clobber). `buf_idx`/`flip_mode`/`flip_arg`
/// are the 7th/8th/9th args, stack-passed on SysV — read `buf_idx` via
/// `syscall_stack_arg(6)`. A double-buffered title flips `buf_idx` (0 or 1); dropping it
/// presented a fixed buffer so the just-rendered frame never scanned out.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SUBMIT_AND_FLIP_COMMAND_BUFFERS,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSubmitAndFlipCommandBuffers"
)]
pub fn sce_gnm_submit_and_flip_command_buffers(
    count: u32,
    dcb_addrs: u64,
    dcb_sizes: u64,
    ccb_addrs: u64,
    ccb_sizes: u64,
    vo_handle: i32,
) -> i32 {
    // 7th arg (buf_idx) is stack-passed; args 0..5 are in registers (vo_handle=arg5).
    let buf_idx = ps4_cpu::syscall_stack_arg(6) as u32;
    record_submit(
        count, dcb_addrs, dcb_sizes, ccb_addrs, ccb_sizes, true, vo_handle, buf_idx,
    );
    0
}

/// `sceGnmSubmitCommandBuffersForWorkload(workload, count, dcb_addrs[], dcb_sizes[],
/// ccb_addrs[], ccb_sizes[])` — the **workload** submit front door (doc-6 Entry 1 §3).
/// A workload is a thin bookkeeping wrapper around the plain submit: an opaque stream
/// id the driver uses for scheduling/profiling on real hardware. Our software GPU has
/// no rings or scheduler, so the workload id carries no semantics we must honor — we
/// log it and funnel the DCB/CCB arrays into the exact same
/// [`record_submit`]/`Executor::run` path as the plain submit.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SUBMIT_COMMAND_BUFFERS_FOR_WORKLOAD,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSubmitCommandBuffersForWorkload"
)]
pub fn sce_gnm_submit_command_buffers_for_workload(
    workload: u32,
    count: u32,
    dcb_addrs: u64,
    dcb_sizes: u64,
    ccb_addrs: u64,
    ccb_sizes: u64,
) -> i32 {
    info!("[GNM] sceGnmSubmitCommandBuffersForWorkload workload={workload}");
    record_submit(
        count, dcb_addrs, dcb_sizes, ccb_addrs, ccb_sizes, false, 0, 0,
    );
    0
}

/// `sceGnmSubmitAndFlipCommandBuffersForWorkload(workload, count, dcb_addrs[],
/// dcb_sizes[], ccb_addrs[], ccb_sizes[], vo_handle, buf_idx, flip_mode, flip_arg)` —
/// the AndFlip variant of the workload submit (doc-6 Entry 1 §3).
///
/// ABI: `workload` shifts every arg by one vs the plain AndFlip, so the six register args
/// are workload=arg0(rdi) .. ccb_sizes=arg5(r9), and the videoout/flip params start one
/// slot later on the stack: vo_handle is the 7th arg (`syscall_stack_arg(6)`), buf_idx the
/// 8th (`syscall_stack_arg(7)`). Threading buf_idx presents the just-rendered scanout
/// buffer instead of a fixed index.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SUBMIT_AND_FLIP_COMMAND_BUFFERS_FOR_WORKLOAD,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSubmitAndFlipCommandBuffersForWorkload"
)]
pub fn sce_gnm_submit_and_flip_command_buffers_for_workload(
    workload: u32,
    count: u32,
    dcb_addrs: u64,
    dcb_sizes: u64,
    ccb_addrs: u64,
    ccb_sizes: u64,
) -> i32 {
    // vo_handle/buf_idx are the 7th/8th args (stack-passed): `workload` occupies arg0, so
    // they sit one slot past where the plain AndFlip has them.
    let vo_handle = ps4_cpu::syscall_stack_arg(6) as i32;
    let buf_idx = ps4_cpu::syscall_stack_arg(7) as u32;
    info!("[GNM] sceGnmSubmitAndFlipCommandBuffersForWorkload workload={workload}");
    record_submit(
        count, dcb_addrs, dcb_sizes, ccb_addrs, ccb_sizes, true, vo_handle, buf_idx,
    );
    0
}

#[allow(clippy::too_many_arguments)]
fn record_submit(
    count: u32,
    dcb_addrs: u64,
    dcb_sizes: u64,
    ccb_addrs: u64,
    ccb_sizes: u64,
    flip: bool,
    vo_handle: i32,
    buf_idx: u32,
) {
    let _span = tracing::debug_span!("gnm_record_submit").entered();
    let prof = ps4_gnm::profile::enabled();
    let t_submit = prof.then(std::time::Instant::now);
    let (dcb_ptrs, dcb_szs, ccb_ptrs, ccb_szs) = (
        read_u64_array(dcb_addrs, count),
        read_u32_array(dcb_sizes, count),
        read_u64_array(ccb_addrs, count),
        read_u32_array(ccb_sizes, count),
    );
    info!(
        "[GNM] {} count={}",
        if flip {
            "sceGnmSubmitAndFlipCommandBuffers"
        } else {
            "sceGnmSubmitCommandBuffers"
        },
        count
    );

    // Per-batch frame index for UNEMUPS4_DUMP_SUBMIT: one bump per submit batch, so
    // our dumps number like the real-PS4 scraper corpus (which numbers per flip).
    let dump_frame = DUMP_FRAME.fetch_add(1, Ordering::Relaxed);

    let t_lock = prof.then(std::time::Instant::now);
    let Ok(mut drv) = driver().lock() else {
        return;
    };
    if let Some(t) = t_lock {
        ps4_gnm::profile::EXEC
            .lock_ns
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
    for i in 0..count as usize {
        let dcb_ptr = dcb_ptrs.get(i).copied().unwrap_or(0);
        let dcb_size = dcb_szs.get(i).copied().unwrap_or(0);
        let ccb_ptr = ccb_ptrs.get(i).copied().unwrap_or(0);
        let ccb_size = ccb_szs.get(i).copied().unwrap_or(0);
        info!(
            "[GNM]   [{}] dcb={:#x} ({} B) ccb={:#x} ({} B)",
            i, dcb_ptr, dcb_size, ccb_ptr, ccb_size
        );
        // `sceGnmSubmitAndFlipCommandBuffers(count, ...)` submits ALL `count` command
        // buffers and flips ONCE — after the last (task-163). Flipping per DCB presented an
        // extra frame per batch: Celeste submits count=2 where dcb[0] is 4 MB of inert NOP
        // padding (zero draws) and dcb[1] holds the frame's real clear+draws. Flipping after
        // the empty dcb[0] presented the videoout image's STALE contents (the previous
        // frame), then dcb[1] cleared, redrew and presented it fresh — an extra stale present
        // per batch that is simply wrong (one submit-and-flip = one presented frame). So flip
        // only on the final DCB; the earlier ones are plain submits whose draws accumulate
        // into the same image.
        let this_flip = flip && i == count as usize - 1;
        // Ground-truth diff dump (task-157): capture the raw submitted DCB/CCB before
        // execution, in the scraper's byte layout, for `decode` to compare vs
        // data/celeste-real-dcb/. No-op unless UNEMUPS4_DUMP_SUBMIT is set.
        dump_submit(dump_frame, i, dcb_ptr, dcb_size, this_flip, false);
        dump_submit(dump_frame, i, ccb_ptr, ccb_size, this_flip, true);
        if this_flip {
            drv.submit_and_flip(dcb_ptr, dcb_size, ccb_ptr, ccb_size, vo_handle, buf_idx);
        } else {
            drv.submit(dcb_ptr, dcb_size, ccb_ptr, ccb_size);
        }
        // Env-gated (UNEMUPS4_PM4_TRACE=1), non-fatal PM4 trace of this range's
        // command buffers. Off by default; decode-only, no execution.
        let range = ps4_gnm::driver::SubmitRange {
            dcb_ptr,
            dcb_size,
            ccb_ptr,
            ccb_size,
            flip: this_flip,
            vo_handle,
            buf_idx,
        };
        unsafe { ps4_gnm::pm4::trace::trace_submit_range(&range) };

        // Present/sync execution (phase 3): only when a present sink is
        // wired (the app registers `GpuManager` at boot). Headless — no display
        // thread, no sink — skips this entirely, so the oracle baselines are
        // unchanged. Runs on the guest thread here (doc-2 §3): decode is Vulkan-free
        // and present crosses the display channel via the sink.
        if let Some(sink) = ps4_core::gpu::present_sink() {
            // Phase 3.5+: Draw mode = the present/sync arms PLUS the SET_*_REG shadow
            // register file (§C7) and the embedded-shader DrawIndexAuto arm. The
            // executor borrows the driver-owned GpuState (`drv.state_mut()`) so
            // register/shader state persists across submits. Only reached when a sink
            // is wired (the app at boot); headless has none, so the oracle baselines
            // are unchanged.
            //
            // LOCK INVARIANT: `drv` (the driver lock) is held across `exec.run`, which
            // blocks on the display channel via the sink — so the display thread must
            // never acquire `driver()` (see ps4_gnm::driver::driver docs). Do not move
            // this off the held lock.
            //
            // The executor resolves every draw's VS/PS bind through the SINGLE
            // provider route (doc-2 §4): a composite chain of embedded FIRST (keeps
            // precedence) then the GCN provider — so a `.sb` GCN bind that the embedded
            // provider defers on is recompiled here, not special-cased into the executor.
            //
            // OWNERSHIP (task-53): the providers, pipeline cache and resource cache are
            // driver-owned, so their state survives across submits — a re-bound shader is a
            // recompile-cache hit, a re-used buffer is not re-uploaded. The GCN provider's
            // dirty-invalidation is drained once per submit before the draws resolve.
            let (state, pipelines, resources, embedded, gcn) = drv.exec_parts();
            let t_dirty = prof.then(std::time::Instant::now);
            let _dirty_span = tracing::debug_span!("apply_dirty").entered();
            if let Some(ds) = ps4_core::dirty::dirty_source() {
                // Drain the dirty source ONCE and feed BOTH consumers (task-178). `take_dirty`
                // is a DRAINING read; the old code drained it in `gcn.drain_dirty` then again
                // in `resources.drain_dirty`, so the SECOND (the resource cache) always saw an
                // empty set and never re-uploaded rewritten dynamic buffers — the MonoGame
                // vertex ring + projection const buffers went STALE, causing the Celeste
                // title-screen frame-alternating garbage. One drain, shared ranges.
                let dirtied = ds.take_dirty();
                // `UNEMUPS4_DIRTY_TRACE=1` reports how many ranges the dirty source hands
                // back per submit. This is the probe that pinned x86jit task-275: it read 0
                // on every submit because `watch_range` silently no-opped above x86jit's
                // 4 GiB watched-page window, while our GPU buffers live around 41 GiB — so
                // the buffer cache could not trust the dirty flag and had to force-re-upload
                // dynamic buffers. Fixed in x86jit `873563f`; the ranges are real now and the
                // force-re-upload workaround is gone. Resolved once; off costs a cached bool.
                static DIRTY_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if *DIRTY_TRACE.get_or_init(|| std::env::var("UNEMUPS4_DIRTY_TRACE").is_ok()) {
                    tracing::info!(
                        "[DIRTY_TRACE] submit returned {} ranges: {:x?}",
                        dirtied.len(),
                        &dirtied[..dirtied.len().min(6)]
                    );
                }
                gcn.apply_dirty(&dirtied);
                resources.apply_dirty(&dirtied);
            }
            if let Some(t) = t_dirty {
                ps4_gnm::profile::EXEC
                    .dirty_ns
                    .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            drop(_dirty_span);
            let providers: [&dyn ps4_gnm::shader::source::ShaderProvider; 2] = [embedded, gcn];
            let chain = ps4_gnm::shader::source::ChainProvider::new(&providers);
            let mut exec = ps4_gnm::exec::Executor::new(
                ps4_gnm::exec::ExecMode::Draw,
                &*sink,
                state,
                &chain,
                pipelines,
                resources,
            );
            unsafe { exec.run(&range) };
        }
    }
    if let Some(t) = t_submit {
        ps4_gnm::profile::EXEC
            .submit_ns
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

/// `sceGnmSubmitDone()` — end-of-batch sync point. Records the batch boundary.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SUBMIT_DONE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSubmitDone"
)]
pub fn sce_gnm_submit_done() -> i32 {
    info!("[GNM] sceGnmSubmitDone");
    if let Ok(mut drv) = driver().lock() {
        drv.submit_done();
    }
    // Signal GPU-completion to any equeue waiter (doc-6 Entry 2). The executor is
    // synchronous, so by submit-done the work is already visible; a guest that gates its
    // frame on a GPU-completion event registered via `sceGnmAddEqEvent` unblocks here.
    crate::libkernel::equeue::signal_gpu_completion();
    0
}
