//! `libSceCommonDialog` HLE — minimal stubs.
//!
//! Common-dialog is the shared substrate the on-screen system dialogs (message,
//! error, IME, NP-profile, save-data list, …) initialize against. A title brings it
//! up once at boot before any dialog is used. There is no system-dialog surface on a
//! native host, so `Initialize` succeeds and `IsUsed` always reports "no dialog in
//! use".

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

#[ps4_syscall(id = SyscallId::SCE_COMMON_DIALOG_INITIALIZE, lib = crate::libs::LIB_SCE_COMMON_DIALOG, name = "sceCommonDialogInitialize")]
pub fn sce_common_dialog_initialize() -> i32 {
    info!("[COMMON_DIALOG] sceCommonDialogInitialize");
    0
}

// Whether any common dialog is currently in use. Nothing is, so report `false` (0).
#[ps4_syscall(id = SyscallId::SCE_COMMON_DIALOG_IS_USED, lib = crate::libs::LIB_SCE_COMMON_DIALOG, name = "sceCommonDialogIsUsed")]
pub fn sce_common_dialog_is_used() -> i32 {
    0
}
