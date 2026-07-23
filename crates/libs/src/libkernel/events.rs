use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_core::kernel::{HandleKind, handle_alloc, handle_resolve};
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::thread;
use std::time::Duration;
use tracing::info;

use super::equeue;

#[ps4_syscall(id = SyscallId::SCE_KERNEL_CREATE_EQUEUE, lib = crate::libs::LIB_KERNEL, name = "sceKernelCreateEqueue")]
pub fn sce_kernel_create_equeue(eq: *mut u64, _name: *const u8) -> i32 {
    // Hand back a kind-tagged arena handle (task-115) through the out-ptr (identity-mapped,
    // doc-2 §1) instead of the old fixed `1`. A retail title registers GPU-completion events
    // against this handle (`sceGnmAddEqEvent`) and waits on it; the workload path signals it
    // on submit-done (doc-6 Entry 2). The handle is an opaque cookie, not a pointer — the
    // registry keys on whatever value the guest round-trips (create → add → wait), so a tagged
    // handle keeps the completion path matching end-to-end while making a stale/foreign handle
    // detectable. `unwrap_or(1)` preserves the old cookie if the table can't allocate.
    let handle = handle_alloc(HandleKind::Equeue).unwrap_or(1);
    // Write the opaque handle through the range-validated, SMC-tracked write seam (task-115):
    // a junk out-ptr fails clean instead of an SMC-invisible host store.
    if let Some(gp) = GuestPtr::<u64>::new(eq as u64) {
        let _ = gp.write(handle as u64);
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_KERNEL_WAIT_EQUEUE, lib = crate::libs::LIB_KERNEL, name = "sceKernelWaitEqueue")]
pub fn sce_kernel_wait_equeue(eq: i32, _ev: u64, num: i32, out: u64, _timeout: u64) -> i32 {
    // The equeue handle create wrote into `*eq` is passed back here by value; the completion
    // registry is keyed on that same value (create → sceGnmAddEqEvent → wait), so use the
    // passed handle directly. A stale/foreign Equeue-tagged handle that no longer resolves
    // reports no completion (falls through to the paced sleep) rather than draining an
    // unrelated queue; a legacy non-tagged value stays lenient (treated as the queue key).
    let eq_handle = eq as i64;
    if ps4_core::kernel::handle_kind(eq) == Some(HandleKind::Equeue)
        && !handle_resolve(eq, Some(HandleKind::Equeue))
    {
        // Bad (freed) equeue handle: no completion, paced sleep.
        thread::sleep(Duration::from_millis(16));
        return 0;
    }

    // Instrumentation (doc-6 Entry 2): does the guest actually gate its frame on a GNM
    // completion event, and is one pending when it waits? The executor is synchronous, so
    // a completion signalled on submit-done is already pending here.
    let triggered = equeue::take_triggered(eq_handle);
    if equeue::has_events() {
        // The guest is COLLECTING GPU completions here — that is the channel it listens on,
        // and it is what makes withholding the raw EOP memory label safe (task-157). Noted on
        // the wait rather than on `sceGnmAddEqEvent`, because registering an event proves
        // nothing: the UE4 title registers one and never waits on it, polling the memory
        // label instead. Waiting is the act that distinguishes them.
        ps4_core::gpu::note_completion_event_registered();
        info!(
            "[EQ] sceKernelWaitEqueue num={num} out={out:#x} triggered={:?} (GNM events registered)",
            triggered
        );
    }

    // Report at most one triggered event to the guest. The out-param is the count of
    // events written (BSD `kevent` returns the number of events); we write the count so a
    // waiter that loops until >0 proceeds. The event array (`_ev`) is left as the guest
    // gave it — Phase A does not synthesize a full `SceKernelEvent` payload; if Celeste
    // reads event data we'll see the next wall and record it (doc-6 Entry 2).
    let count = if triggered.is_some() { 1i32 } else { 0i32 };
    // Write the event count through the range-validated, SMC-tracked write seam (task-115).
    if let Some(gp) = GuestPtr::<i32>::new(out) {
        let _ = gp.write(count.max(0));
    }

    // No completion pending: fall back to the VSync-paced sleep (approx 60 FPS) so a boot
    // loop that spins on the wait doesn't burn the CPU — the pre-existing behaviour.
    if triggered.is_none() {
        thread::sleep(Duration::from_millis(16));
    }
    0
}

/// `sceGnmAddEqEvent(eq, event_type, id)` — register a GPU-completion event on an event
/// queue (doc-6 Entry 2). The workload submit-done path signals this so a subsequent
/// `sceKernelWaitEqueue` reports the completion. Tracked in the process-global equeue
/// registry; returns success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_ADD_EQ_EVENT,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmAddEqEvent"
)]
pub fn sce_gnm_add_eq_event(eq: i64, event_type: i32, id: i64) -> i32 {
    info!("[GNM] sceGnmAddEqEvent eq={eq} type={event_type} id={id:#x}");
    equeue::add_event(eq, event_type, id);
    0
}

/// `sceGnmDeleteEqEvent(eq, id)` — unregister a GPU-completion event (doc-6 Entry 2).
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DELETE_EQ_EVENT,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDeleteEqEvent"
)]
pub fn sce_gnm_delete_eq_event(eq: i64, id: i64) -> i32 {
    info!("[GNM] sceGnmDeleteEqEvent eq={eq} id={id:#x}");
    equeue::delete_event(eq, id);
    0
}

/// `sceGnmGetEqEventType(event) -> type` — read the GNM event type back. On real hardware
/// this reads the type out of a `SceKernelEvent` the wait returned; here we look the type
/// up in the registry keyed on the event's `id`. Phase A: the `event` arg is the guest
/// cookie/id (the same `id` passed to AddEqEvent), so return the registered type or 0.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_GET_EQ_EVENT_TYPE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmGetEqEventType"
)]
pub fn sce_gnm_get_eq_event_type(id: i64) -> i32 {
    // The queue handle isn't passed here, and the create handle is now an arena cookie rather
    // than a fixed `1` (task-115), so look the type up by `id` alone.
    let t = equeue::event_type_by_id(id).unwrap_or(0);
    info!("[GNM] sceGnmGetEqEventType id={id:#x} -> {t}");
    t
}
