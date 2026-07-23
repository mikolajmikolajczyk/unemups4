//! libSceGnmDriver **workload** stream lifecycle + flip-and-submit-done (doc-6 Entry 1 §3).
//!
//! A workload is a thin bookkeeping wrapper the newer GNM submission model puts around the
//! plain submit: `CreateWorkloadStream` mints an opaque stream id, `Begin`/`End` bracket a
//! group of submits, `DingDong` is a compute doorbell, and `RequestFlipAndSubmitDone`
//! requests a present + end-of-batch sync. On real hardware these feed the GPU scheduler /
//! ring doorbell; a **software GPU with no rings or scheduler** honors none of that
//! (doc-6 Entry 1 §3). So the lifecycle calls are bookkeeping/no-op success — the actual
//! work reaches the executor through `Submit*ForWorkload` (in [`super::submit`]), which
//! funnels the same `SubmitRange` into `Executor::run` as the plain submit.
//!
//! The one non-trivial handler is `CreateWorkloadStream`: it must write a non-zero opaque
//! stream id through the out-ptr (a guest that gets 0 may treat it as failure). We hand out
//! a monotonically increasing id and return 0 (success).

use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;

/// Next opaque workload-stream id. Starts at 1 so the id handed out is always non-zero (a
/// guest may treat a 0 stream id as an allocation failure).
static NEXT_WORKLOAD_STREAM: AtomicU64 = AtomicU64::new(1);

/// `sceGnmCreateWorkloadStream(name, out_stream)` — mint an opaque workload-stream id and
/// write it through `out_stream` (identity-mapped, doc-2 §1). Returns 0 (success). The id
/// is bookkeeping only — no ring or scheduler backs it (doc-6 Entry 1 §3).
#[ps4_syscall(
    id = SyscallId::SCE_GNM_CREATE_WORKLOAD_STREAM,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmCreateWorkloadStream"
)]
pub fn sce_gnm_create_workload_stream(_name: u64, out_stream: u64) -> i32 {
    let id = NEXT_WORKLOAD_STREAM.fetch_add(1, Ordering::Relaxed);
    info!("[GNM] sceGnmCreateWorkloadStream -> stream={id}");
    // Write the opaque id through the range-validated, SMC-tracked write seam (task-115): a
    // junk out-ptr (POSIX-alias register garbage) fails clean instead of faulting the host or
    // leaving an SMC-invisible store on a page the guest may later execute.
    if let Some(gp) = GuestPtr::<u64>::new(out_stream) {
        let _ = gp.write(id);
    }
    0
}

/// `sceGnmDestroyWorkloadStream(stream)` — release a workload stream. No-op success (the
/// id backs no real resource).
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DESTROY_WORKLOAD_STREAM,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDestroyWorkloadStream"
)]
pub fn sce_gnm_destroy_workload_stream(_stream: u64) -> i32 {
    0
}

/// `sceGnmBeginWorkload(stream, out_workload)` — begin a workload on a stream, minting a
/// workload id. Bookkeeping only (doc-6 Entry 1 §3): write a non-zero workload id through
/// the out-ptr and return success. Reuses the same monotonic counter as the stream id — a
/// guest treats both as opaque handles.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_BEGIN_WORKLOAD,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmBeginWorkload"
)]
pub fn sce_gnm_begin_workload(_stream: u64, out_workload: u64) -> i32 {
    let id = NEXT_WORKLOAD_STREAM.fetch_add(1, Ordering::Relaxed);
    if let Some(gp) = GuestPtr::<u64>::new(out_workload) {
        let _ = gp.write(id);
    }
    0
}

/// `sceGnmEndWorkload(workload)` — end a workload. No-op success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_END_WORKLOAD,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmEndWorkload"
)]
pub fn sce_gnm_end_workload(_workload: u64) -> i32 {
    0
}

/// `sceGnmDingDongForWorkload(workload, ring_id, next_offs_dw)` — a compute-ring doorbell
/// scoped to a workload. A software GPU has no rings, so the doorbell is a no-op
/// (doc-6 Entry 1 §3). Returns success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DING_DONG_FOR_WORKLOAD,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDingDongForWorkload"
)]
pub fn sce_gnm_ding_dong_for_workload(_workload: u64, _ring_id: u32, _next_offs_dw: u32) -> i32 {
    0
}

/// `sceGnmRequestFlipAndSubmitDone(...)` — request a present + end-of-batch sync. The flip
/// itself crosses through the present sink on the submit-and-flip path; here we only signal
/// GPU-completion to any equeue waiter (doc-6 Entry 2), matching `sceGnmSubmitDone`.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_REQUEST_FLIP_AND_SUBMIT_DONE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmRequestFlipAndSubmitDone"
)]
pub fn sce_gnm_request_flip_and_submit_done() -> i32 {
    info!("[GNM] sceGnmRequestFlipAndSubmitDone");
    crate::libkernel::equeue::signal_gpu_completion();
    0
}

/// `sceGnmRequestFlipAndSubmitDoneForWorkload(...)` — the workload-scoped variant. Same
/// semantics as [`sce_gnm_request_flip_and_submit_done`] for a software GPU.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_REQUEST_FLIP_AND_SUBMIT_DONE_FOR_WORKLOAD,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmRequestFlipAndSubmitDoneForWorkload"
)]
pub fn sce_gnm_request_flip_and_submit_done_for_workload() -> i32 {
    info!("[GNM] sceGnmRequestFlipAndSubmitDoneForWorkload");
    crate::libkernel::equeue::signal_gpu_completion();
    0
}
