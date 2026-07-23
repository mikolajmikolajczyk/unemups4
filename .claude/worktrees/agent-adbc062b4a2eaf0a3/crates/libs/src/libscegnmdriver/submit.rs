//! libSceGnmDriver submit/flip entry points (doc-4 §1). The submit stubs
//! extract the DCB/CCB pointer/size ranges from guest memory and record them into
//! the `GnmDriver` (in `ps4-gnm`), which retains them for the PM4 trace decoder.
//! No PM4 is decoded or executed here — everything is log-and-return-
//! success. Guest ptrs are identity-mapped (guest ptr == host ptr, doc-2 §1), so
//! the arrays are read directly.

use crate::context::NativeContext;
use ps4_core::memory::MemoryAccessExt;
use ps4_gnm::driver::driver;
use ps4_gnm::idmem::IdentityMem;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

/// Read `count` 64-bit command-buffer GPU addresses from a guest array pointer.
/// The Gnm submit ABI passes the DCB/CCB address arrays as `void*[]` (64-bit
/// GPU VAs); an identity-mapped OpenOrbis `malloc` pointer lives above 4 GB, so
/// reading them as 32-bit truncated the buffer to all-zeros. Resolved through the
/// identity mapping (guest ptr == host ptr, doc-2 §1).
fn read_u64_array(ptr: u64, count: u32) -> Vec<u64> {
    IdentityMem
        .read_array::<u64>(ptr, count as usize)
        .unwrap_or_default()
}

/// Read `count` u32 sizes from a guest array pointer. The size arrays are
/// `uint32_t*` on the Gnm ABI (byte counts), read here via the identity mapping.
fn read_u32_array(ptr: u64, count: u32) -> Vec<u32> {
    IdentityMem
        .read_array::<u32>(ptr, count as usize)
        .unwrap_or_default()
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
    record_submit(count, dcb_addrs, dcb_sizes, ccb_addrs, ccb_sizes, false);
    0
}

/// `sceGnmSubmitAndFlipCommandBuffers(count, dcb_addrs[], dcb_sizes[], ccb_addrs[],
/// ccb_sizes[], vo_handle, buf_idx, flip_mode, flip_arg)`. Records the DCB/CCB
/// pairs marked as flip-carrying; returns success. The actual scanout flip is
/// wired in a later phase.
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
) -> i32 {
    record_submit(count, dcb_addrs, dcb_sizes, ccb_addrs, ccb_sizes, true);
    0
}

fn record_submit(
    count: u32,
    dcb_addrs: u64,
    dcb_sizes: u64,
    ccb_addrs: u64,
    ccb_sizes: u64,
    flip: bool,
) {
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

    let Ok(mut drv) = driver().lock() else {
        return;
    };
    for i in 0..count as usize {
        let dcb_ptr = dcb_ptrs.get(i).copied().unwrap_or(0);
        let dcb_size = dcb_szs.get(i).copied().unwrap_or(0);
        let ccb_ptr = ccb_ptrs.get(i).copied().unwrap_or(0);
        let ccb_size = ccb_szs.get(i).copied().unwrap_or(0);
        info!(
            "[GNM]   [{}] dcb={:#x} ({} B) ccb={:#x} ({} B)",
            i, dcb_ptr, dcb_size, ccb_ptr, ccb_size
        );
        if flip {
            drv.submit_and_flip(dcb_ptr, dcb_size, ccb_ptr, ccb_size);
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
            flip,
        };
        unsafe { ps4_gnm::pm4::trace::trace_submit_range(&range) };

        // Present/sync execution (phase 3): only when a present sink is
        // wired (the app registers `GpuManager` at boot). Headless — no display
        // thread, no sink — skips this entirely, so the oracle baselines are
        // unchanged. Runs on the guest thread here (doc-4 §3): decode is Vulkan-free
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
            // provider route (doc-4 §4): a composite chain of embedded FIRST (keeps
            // precedence) then the GCN provider — so a `.sb` GCN bind that the embedded
            // provider defers on is recompiled here, not special-cased into the executor.
            //
            // OWNERSHIP (task-53): the providers, pipeline cache and resource cache are
            // driver-owned, so their state survives across submits — a re-bound shader is a
            // recompile-cache hit, a re-used buffer is not re-uploaded. The GCN provider's
            // dirty-invalidation is drained once per submit before the draws resolve.
            let (state, pipelines, resources, embedded, gcn) = drv.exec_parts();
            if let Some(ds) = ps4_core::dirty::dirty_source() {
                gcn.drain_dirty(ds.as_ref());
                resources.drain_dirty(ds.as_ref());
            }
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
    0
}
