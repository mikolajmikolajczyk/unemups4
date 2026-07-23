//! Gnm's **resource registration and capture** surface — the half of libSceGnmDriver that
//! exists for Razor and the PA tools, not for rendering.
//!
//! Nothing in this file moves a pixel. `sceGnmRegisterOwner`/`RegisterResource` attach
//! human-readable names to buffers and textures so a GPU capture can label them; the
//! `*InProgress` calls ask whether a capture or thread trace is running. On hardware these
//! answer a debugger that is attached to the process. Here there is no debugger, and the
//! diagnostic we actually act on is our own PM4 trace.
//!
//! The split matters for the smoke loop (doc-4): a title that calls these is not asking the
//! GPU for anything, so answering them cheaply is right. Real draw and dispatch entry
//! points are deliberately *not* stubbed alongside them — a missing `sceGnmDrawIndirect`
//! must keep announcing itself as a missing symbol rather than silently drawing nothing,
//! because that one is the difference between a rendered frame and a black one.
//!
//! Registrations are accepted and dropped. We keep no table: nothing here reads it back
//! except `sceGnmFindResourcesPublic`, which is a tool-facing query no title's own logic
//! depends on, and which refuses for exactly that reason.

use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

/// A refusal for the tool-facing queries we cannot answer truthfully. Gnm returns negative
/// SCE codes; we do not have the documented values for this library and will not invent one
/// that reads as verified.
const GNM_TOOL_REFUSED: i32 = -1;

/// The one owner handle we ever hand out. Owners are opaque to the title — minted by us,
/// passed back to us — so a single non-zero value is enough, and a zero handle (which a
/// title may read as failure) never escapes.
const GNM_OWNER_HANDLE: u32 = 1;

/// The one resource handle we ever hand out. Same reasoning as [`GNM_OWNER_HANDLE`].
const GNM_RESOURCE_HANDLE: u32 = 1;

/// Write an opaque handle through a guest out-pointer via the range-validated seam, so
/// register-garbage fails clean instead of faulting the host.
fn write_handle(out_ptr: u64, handle: u32) {
    if let Some(gp) = GuestPtr::<u32>::new(out_ptr) {
        let _ = gp.write(handle);
    }
}

/// `sceGnmRegisterOwner(OwnerHandle *outHandle, const char *name)` — name a group of
/// resources for a capture tool. Accepted, and a non-zero handle is written back: the title
/// stores it and passes it to every subsequent `RegisterResource`, so a zero here would
/// propagate as a bad owner into calls that follow.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_REGISTER_OWNER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmRegisterOwner"
)]
pub fn sce_gnm_register_owner(out_handle: u64, _name: u64) -> i32 {
    write_handle(out_handle, GNM_OWNER_HANDLE);
    0
}

/// `sceGnmRegisterResource(ResourceHandle *outHandle, OwnerHandle owner, const void *mem,
/// size_t size, const char *name, ResourceType type, uint64_t userData)` — label one
/// allocation. Accepted; the label is dropped. Only the out-pointer is read: the arguments
/// past it describe a table we do not keep (and the syscall seam carries six at most).
#[ps4_syscall(
    id = SyscallId::SCE_GNM_REGISTER_RESOURCE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmRegisterResource"
)]
pub fn sce_gnm_register_resource(out_handle: u64) -> i32 {
    write_handle(out_handle, GNM_RESOURCE_HANDLE);
    0
}

/// `sceGnmRegisterGdsResource(...)` — the same, for a GDS range.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_REGISTER_GDS_RESOURCE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmRegisterGdsResource"
)]
pub fn sce_gnm_register_gds_resource(out_handle: u64) -> i32 {
    write_handle(out_handle, GNM_RESOURCE_HANDLE);
    0
}

/// `sceGnmUnregisterResource(ResourceHandle)` — drop one label.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_UNREGISTER_RESOURCE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmUnregisterResource"
)]
pub fn sce_gnm_unregister_resource() -> i32 {
    0
}

/// `sceGnmUnregisterOwnerAndResources(OwnerHandle)` — drop an owner and everything under it.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_UNREGISTER_OWNER_AND_RESOURCES,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmUnregisterOwnerAndResources"
)]
pub fn sce_gnm_unregister_owner_and_resources() -> i32 {
    0
}

/// `sceGnmUnregisterAllResourcesForOwner(OwnerHandle)` — keep the owner, drop its contents.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_UNREGISTER_ALL_RESOURCES_FOR_OWNER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmUnregisterAllResourcesForOwner"
)]
pub fn sce_gnm_unregister_all_resources_for_owner() -> i32 {
    0
}

/// `sceGnmSetResourceUserData(ResourceHandle, uint64_t)` — attach a title-defined tag to a
/// registered resource. Accepted and dropped, like the registration it decorates.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_RESOURCE_USER_DATA,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetResourceUserData"
)]
pub fn sce_gnm_set_resource_user_data() -> i32 {
    0
}

/// `sceGnmSetResourceRegistrationUserMemory(void *mem, size_t size, uint32_t numResources)`
/// — hand the driver a scratch block to hold the registration table in. Accepted and
/// ignored: our table does not exist, so it needs no memory. The title keeps ownership of
/// the block it passed and can free it whenever it likes.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_RESOURCE_REGISTRATION_USER_MEMORY,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetResourceRegistrationUserMemory"
)]
pub fn sce_gnm_set_resource_registration_user_memory() -> i32 {
    0
}

/// `sceGnmQueryResourceRegistrationUserMemoryRequirements(SizeAlign *out, ...)` — how big
/// that block must be. Refused rather than answered: the out-struct's layout is not one we
/// have verified, and a wrong size written into it becomes an allocation the title makes on
/// our word. A refusal costs the title its resource-name table, which nothing but a capture
/// tool would have read.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_QUERY_RESOURCE_REGISTRATION_USER_MEMORY_REQUIREMENTS,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmQueryResourceRegistrationUserMemoryRequirements"
)]
pub fn sce_gnm_query_resource_registration_user_memory_requirements() -> i32 {
    GNM_TOOL_REFUSED
}

/// `sceGnmFindResourcesPublic(...)` — enumerate what has been registered. Refused: we keep
/// no table to enumerate. This is a tool-facing query; a title's own rendering never
/// depends on the answer.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_FIND_RESOURCES_PUBLIC,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmFindResourcesPublic"
)]
pub fn sce_gnm_find_resources_public() -> i32 {
    GNM_TOOL_REFUSED
}

// --- Capture / trace state: asked every frame by engines that support Razor. ---

/// `sceGnmDriverCaptureInProgress()` — is a GPU capture being taken right now? No. This is
/// polled per frame, and "no" is both the truth and the answer that keeps the title on its
/// normal path.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRIVER_CAPTURE_IN_PROGRESS,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDriverCaptureInProgress"
)]
pub fn sce_gnm_driver_capture_in_progress() -> i32 {
    0
}

/// `sceGnmDriverTraceInProgress()` — is a thread trace running? No.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRIVER_TRACE_IN_PROGRESS,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDriverTraceInProgress"
)]
pub fn sce_gnm_driver_trace_in_progress() -> i32 {
    0
}

/// `sceGnmDriverTriggerCapture(const char *filename)` — take a Razor capture. Refused: no
/// capture is produced, and reporting success would leave a title waiting for a file that
/// never appears.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRIVER_TRIGGER_CAPTURE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDriverTriggerCapture"
)]
pub fn sce_gnm_driver_trigger_capture() -> i32 {
    GNM_TOOL_REFUSED
}

/// `sceGnmIsUserPaEnabled()` — is user performance analysis on? No.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_IS_USER_PA_ENABLED,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmIsUserPaEnabled"
)]
pub fn sce_gnm_is_user_pa_enabled() -> i32 {
    0
}

// --- Mip statistics: which mip levels the GPU actually sampled, for streaming budgets. ---

/// `sceGnmSetupMipStatsReport(...)` — arm mip-level statistics collection. Accepted; the
/// hardware counters behind it do not exist here.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SETUP_MIP_STATS_REPORT,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetupMipStatsReport"
)]
pub fn sce_gnm_setup_mip_stats_report() -> i32 {
    0
}

/// `sceGnmDisableMipStatsReport()` — disarm it.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DISABLE_MIP_STATS_REPORT,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDisableMipStatsReport"
)]
pub fn sce_gnm_disable_mip_stats_report() -> i32 {
    0
}

/// `sceGnmRequestMipStatsReportAndReset()` — collect the counters. Refused: there are none,
/// and a zeroed report would tell a streaming system that no mip was ever sampled, which is
/// a *measurement* it may act on by evicting textures.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_REQUEST_MIP_STATS_REPORT_AND_RESET,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmRequestMipStatsReportAndReset"
)]
pub fn sce_gnm_request_mip_stats_report_and_reset() -> i32 {
    GNM_TOOL_REFUSED
}

/// `sceGnmDebugHardwareStatus(...)` — dump GPU block status for a hang report. Refused; the
/// blocks it reports on are Liverpool hardware we do not emulate.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DEBUG_HARDWARE_STATUS,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDebugHardwareStatus"
)]
pub fn sce_gnm_debug_hardware_status() -> i32 {
    GNM_TOOL_REFUSED
}
