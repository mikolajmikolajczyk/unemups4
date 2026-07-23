//! libSceGnmDriver hardware-state-init preamble builders, submit gating, debug
//! markers, and cache-flush stubs (log-and-return-success). These write HW-state /
//! marker PM4 into the guest cmdbuf; none is emitted here — everything is a no-op
//! that returns success (or the dword count consumed, for the init-HW-state family).

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

/// `sceGnmAreSubmitsAllowed()` — homebrew polls this before submitting. Return 1
/// (allowed) so a boot loop that gates on it proceeds.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_ARE_SUBMITS_ALLOWED,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmAreSubmitsAllowed"
)]
pub fn sce_gnm_are_submits_allowed() -> i32 {
    1
}

/// `sceGnmDrawInitDefaultHardwareState(cmdbuf, size)` — writes the default HW-state
/// PM4 preamble into the cmdbuf. Return the dword count consumed (`size`) so the
/// caller advances its write cursor. No PM4 written yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INIT_DEFAULT_HARDWARE_STATE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawInitDefaultHardwareState"
)]
pub fn sce_gnm_draw_init_default_hardware_state(_cmdbuf: u64, size: u32) -> u32 {
    size
}

#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INIT_DEFAULT_HARDWARE_STATE175,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawInitDefaultHardwareState175"
)]
pub fn sce_gnm_draw_init_default_hardware_state175(_cmdbuf: u64, size: u32) -> u32 {
    size
}

#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INIT_DEFAULT_HARDWARE_STATE200,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawInitDefaultHardwareState200"
)]
pub fn sce_gnm_draw_init_default_hardware_state200(_cmdbuf: u64, size: u32) -> u32 {
    size
}

#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INIT_DEFAULT_HARDWARE_STATE350,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawInitDefaultHardwareState350"
)]
pub fn sce_gnm_draw_init_default_hardware_state350(_cmdbuf: u64, size: u32) -> u32 {
    size
}

/// `sceGnmInsertPushMarker(cmdbuf, size, marker)` — debug marker; no-op success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_INSERT_PUSH_MARKER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmInsertPushMarker"
)]
pub fn sce_gnm_insert_push_marker(_cmdbuf: u64, _size: u32, _marker: u64) -> i32 {
    0
}

/// `sceGnmInsertPopMarker(cmdbuf, size)` — debug marker; no-op success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_INSERT_POP_MARKER,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmInsertPopMarker"
)]
pub fn sce_gnm_insert_pop_marker(_cmdbuf: u64, _size: u32) -> i32 {
    0
}

/// `sceGnmInsertWaitFlipDone(cmdbuf, size, vo_handle, buf_idx)` — inserts a
/// wait-for-flip PM4 packet; no-op success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_INSERT_WAIT_FLIP_DONE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmInsertWaitFlipDone"
)]
pub fn sce_gnm_insert_wait_flip_done(
    _cmdbuf: u64,
    _size: u32,
    _vo_handle: i32,
    _buf_idx: u32,
) -> i32 {
    0
}

/// `sceGnmFlushGarlic()` — flushes the garlic (GPU-side) memory cache; no-op success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_FLUSH_GARLIC,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmFlushGarlic"
)]
pub fn sce_gnm_flush_garlic() -> i32 {
    0
}
