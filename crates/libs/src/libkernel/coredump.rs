//! `libSceCoredump` HLE — the crash-dump hook, accepted and never called.
//!
//! A title registers a coredump handler at boot so that, when the process faults, the
//! system runs it to attach extra context (build id, level name, allocator state) to the
//! dump Sony collects. Registration is bookkeeping; the handler only ever runs from inside
//! a crash the system is already handling.
//!
//! unemups4 has no coredump path at all — a guest fault surfaces through the fault reporter
//! (task-113.2), which names the RIP, the VMA and the import, and that is the diagnostic we
//! actually act on. So registration succeeds and the handler stays unused. Writing user
//! data refuses, because there is no dump to write it into; that call is only reachable
//! from within a handler we never invoke.

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

/// `sceCoredumpRegisterCoredumpHandler(handler, stackSize, userdata)` — remember a handler
/// for a crash dump that will never be taken. Succeeds: failing here would look like a
/// broken system service to a title that has not crashed yet.
#[ps4_syscall(
    id = SyscallId::SCE_COREDUMP_REGISTER_COREDUMP_HANDLER,
    lib = crate::libs::LIB_KERNEL,
    name = "sceCoredumpRegisterCoredumpHandler"
)]
pub fn sce_coredump_register_coredump_handler() -> i32 {
    0
}

/// `sceCoredumpWriteUserData(...)` — attach data to the dump in progress. Refused: this is
/// only callable from inside a coredump handler, and no handler ever runs here.
#[ps4_syscall(
    id = SyscallId::SCE_COREDUMP_WRITE_USER_DATA,
    lib = crate::libs::LIB_KERNEL,
    name = "sceCoredumpWriteUserData"
)]
pub fn sce_coredump_write_user_data() -> i32 {
    -1
}
