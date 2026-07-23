//! `libSceIme` HLE — minimal stubs.
//!
//! Ime is the PS4 on-screen-keyboard / text-input subsystem. A game wires it up
//! at boot (register a keyboard, install callbacks) even when it never asks the
//! player to type: there is no on-screen keyboard on a native host, so these
//! stubs make each call succeed with a benign default and report "no text
//! event". Out-params are guarded with `is_guest_ptr` before deref.

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::ffi::c_void;
use tracing::info;

#[ps4_syscall(id = SyscallId::SCE_IME_KEYBOARD_OPEN, lib = crate::libs::LIB_SCE_IME, name = "sceImeKeyboardOpen")]
pub fn sce_ime_keyboard_open(user_id: i32, _param: *const c_void) -> i32 {
    info!("[IME] sceImeKeyboardOpen user_id={}", user_id);
    0
}

// sceImeUpdate(SceImeEventHandler handler) — per-frame IME pump. On real HW it drains
// the on-screen-keyboard event queue and invokes `handler` for each event (key press,
// text change, keyboard open/close). With no host keyboard subsystem there is never an
// event, so the correct no-op is to return SCE_OK (0) without calling the handler:
// "nothing happened this frame". Celeste's Sce.PlayStation4 layer calls this every frame
// once it advances past attract (task-170); a missing symbol here aborted the process.
#[ps4_syscall(id = SyscallId::SCE_IME_UPDATE, lib = crate::libs::LIB_SCE_IME, name = "sceImeUpdate")]
pub fn sce_ime_update(_handler: *const c_void) -> i32 {
    0
}
