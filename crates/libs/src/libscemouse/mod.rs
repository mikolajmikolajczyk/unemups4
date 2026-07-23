//! `libSceMouse` HLE — minimal stubs.
//!
//! Mouse is an optional PS4 input device. A game initializes the subsystem at
//! boot but no USB mouse is attached on a native host, so these stubs make init
//! succeed and report "no mouse connected" for anything that queries a device.
//! Out-params are guarded with `is_guest_ptr` before deref.

use crate::context::NativeContext;
use ps4_core::kernel::{HandleKind, handle_alloc};
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

#[ps4_syscall(id = SyscallId::SCE_MOUSE_INIT, lib = crate::libs::LIB_SCE_MOUSE, name = "sceMouseInit")]
pub fn sce_mouse_init() -> i32 {
    info!("[MOUSE] sceMouseInit");
    0
}

/// `sceMouseOpen(userId, type, index, param)` — opens a mouse port and returns a
/// non-negative handle. A game opens the port at boot even with no USB mouse attached;
/// returning a valid handle lets init proceed, and a later `sceMouseRead` reports the
/// not-connected state. The handle is opaque to the guest — a fixed positive id is
/// enough for the single-port stub.
#[ps4_syscall(id = SyscallId::SCE_MOUSE_OPEN, lib = crate::libs::LIB_SCE_MOUSE, name = "sceMouseOpen")]
pub fn sce_mouse_open(_user_id: i32, _type: i32, _index: i32, _param: u64) -> i32 {
    info!("[MOUSE] sceMouseOpen");
    // Kind-tagged arena handle (task-115) rather than a fixed `1`. There is no `sceMouseClose`
    // handler yet, so the handle is never freed — a single boot-time open, so it does not leak
    // in any meaningful sense. `unwrap_or(1)` preserves the old behaviour if the table can't
    // allocate.
    handle_alloc(HandleKind::Mouse).unwrap_or(1)
}

/// `sceMouseRead(handle, data*, num)` — dequeue up to `num` buffered mouse samples into
/// `data`, returning the number actually read (0..num) or a negative error. No USB mouse
/// is attached on a native host, so there are never any samples: return 0 ("no data this
/// poll") without writing the buffer. A game polling an optional mouse each frame simply
/// sees no input. PROVISIONAL: a title that gates on `data[0].connected` rather than the
/// return count would want one zeroed SceMouseData written with connected=0 — Celeste
/// only polls optionally, so 0 is sufficient (task-170).
#[ps4_syscall(id = SyscallId::SCE_MOUSE_READ, lib = crate::libs::LIB_SCE_MOUSE, name = "sceMouseRead")]
pub fn sce_mouse_read(_handle: i32, _data_ptr: *mut u8, _num: i32) -> i32 {
    0
}
