//! `libSceNpTrophy` HLE — minimal boot-unblock stubs.
//!
//! The trophy subsystem is the PS4 achievement API. A game wires it up during boot
//! (create a context + handle, register the trophy set) even before the player earns
//! anything. unemups4 is a research emulator with no NP/PSN backend and no trophy store,
//! so these are **boot-unblock stubs**: each call succeeds with a benign default, the
//! two `Create*` calls hand back a non-zero opaque id through their out-ptr (a guest may
//! treat a 0 handle as an allocation failure), and nothing is persisted.
//!
//! This is not GNM work — it surfaced as the wall immediately *after* Celeste's first
//! GNM draw (`SetVsShader` → `SetPsShader350` → `DrawIndexAuto`), upstream of its first
//! command-buffer submit, exactly as `sceKernelIsNeoMode` did in Phase A. Stubbing it is
//! the pull-driven move that lets the boot reach the executor (task-113.4.1 AC#4/#5).
//! Signatures follow `data/oo_sdk/include/orbis/NpTrophy.h`.

use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::sync::atomic::{AtomicI32, Ordering};
use tracing::info;

/// Next opaque trophy context/handle id. Starts at 1 so a handed-out id is never 0 (a
/// guest may read 0 as failure). Contexts and handles share the counter — both are
/// opaque `int32_t` the guest only round-trips back into destroy/register.
static NEXT_TROPHY_ID: AtomicI32 = AtomicI32::new(1);

/// Write a non-zero id through a guest out-ptr via the range-validated, SMC-tracked write
/// seam (task-115) — register-garbage pointers fail clean instead of faulting the host.
fn write_id(out_ptr: u64, id: i32) {
    if let Some(gp) = GuestPtr::<i32>::new(out_ptr) {
        let _ = gp.write(id);
    }
}

/// `sceNpTrophyCreateContext(int32_t *context, int32_t user, uint32_t, uint64_t)` — mint an
/// opaque trophy context id through the out-ptr. Success.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TROPHY_CREATE_CONTEXT,
    lib = crate::libs::LIB_SCE_NP_TROPHY,
    name = "sceNpTrophyCreateContext"
)]
pub fn sce_np_trophy_create_context(context: u64, user: i32, _unk: u32, _unk2: u64) -> i32 {
    let id = NEXT_TROPHY_ID.fetch_add(1, Ordering::Relaxed);
    info!("[NPTROPHY] sceNpTrophyCreateContext user={user} -> context={id}");
    write_id(context, id);
    0
}

/// `sceNpTrophyCreateHandle(int32_t *handle)` — mint an opaque trophy handle id. Success.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TROPHY_CREATE_HANDLE,
    lib = crate::libs::LIB_SCE_NP_TROPHY,
    name = "sceNpTrophyCreateHandle"
)]
pub fn sce_np_trophy_create_handle(handle: u64) -> i32 {
    let id = NEXT_TROPHY_ID.fetch_add(1, Ordering::Relaxed);
    info!("[NPTROPHY] sceNpTrophyCreateHandle -> handle={id}");
    write_id(handle, id);
    0
}

/// `sceNpTrophyDestroyContext(int32_t context)` — release a context. No-op success.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TROPHY_DESTROY_CONTEXT,
    lib = crate::libs::LIB_SCE_NP_TROPHY,
    name = "sceNpTrophyDestroyContext"
)]
pub fn sce_np_trophy_destroy_context(_context: i32) -> i32 {
    0
}

/// `sceNpTrophyDestroyHandle(int32_t handle)` — release a handle. No-op success.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TROPHY_DESTROY_HANDLE,
    lib = crate::libs::LIB_SCE_NP_TROPHY,
    name = "sceNpTrophyDestroyHandle"
)]
pub fn sce_np_trophy_destroy_handle(_handle: i32) -> i32 {
    0
}

/// `sceNpTrophyRegisterContext(int32_t context, int32_t handle, uint64_t)` — register the
/// title's trophy set. No backend to register into; report success so boot proceeds.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TROPHY_REGISTER_CONTEXT,
    lib = crate::libs::LIB_SCE_NP_TROPHY,
    name = "sceNpTrophyRegisterContext"
)]
pub fn sce_np_trophy_register_context(_context: i32, _handle: i32, _unk: u64) -> i32 {
    info!("[NPTROPHY] sceNpTrophyRegisterContext");
    0
}

/// `sceNpTrophyGetTrophyUnlockState(context, handle, flags*, count*)` — report the unlock
/// bitmap. With no trophy store, report "nothing unlocked": zero the guest out-params when
/// they are plausible pointers, and return success. The exact `SceNpTrophyFlagArray` layout
/// isn't in the SDK header (declared arg-less), so only the leading dwords are zeroed
/// defensively; a game that reads a full bitmap sees all-locked, which is correct here.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TROPHY_GET_TROPHY_UNLOCK_STATE,
    lib = crate::libs::LIB_SCE_NP_TROPHY,
    name = "sceNpTrophyGetTrophyUnlockState"
)]
pub fn sce_np_trophy_get_trophy_unlock_state(
    _context: i32,
    _handle: i32,
    flags: u64,
    count: u64,
) -> i32 {
    info!("[NPTROPHY] sceNpTrophyGetTrophyUnlockState -> none unlocked");
    // Zero the flag bitmap's leading dword and the count through the range-validated,
    // SMC-tracked write seam (task-115) when they resolve as guest pointers.
    if let Some(gp) = GuestPtr::<u32>::new(flags) {
        let _ = gp.write(0);
    }
    if let Some(gp) = GuestPtr::<u32>::new(count) {
        let _ = gp.write(0);
    }
    0
}

/// `sceNpTrophyUnlockTrophy(context, handle, trophyId, platinumId*)` — the game asks to
/// unlock a trophy. No store to unlock into; report success and, when `platinum` is a
/// plausible out-ptr, write the "no platinum granted" sentinel (`SCE_NP_TROPHY_INVALID_TROPHY_ID`
/// is -1) so a guest that checks the platinum result sees "not unlocked".
#[ps4_syscall(
    id = SyscallId::SCE_NP_TROPHY_UNLOCK_TROPHY,
    lib = crate::libs::LIB_SCE_NP_TROPHY,
    name = "sceNpTrophyUnlockTrophy"
)]
pub fn sce_np_trophy_unlock_trophy(
    _context: i32,
    _handle: i32,
    trophy_id: i32,
    platinum: u64,
) -> i32 {
    info!("[NPTROPHY] sceNpTrophyUnlockTrophy trophy={trophy_id}");
    // -1 == SCE_NP_TROPHY_INVALID_TROPHY_ID: no platinum was granted by this unlock. Written
    // through the range-validated, SMC-tracked write seam (task-115).
    if let Some(gp) = GuestPtr::<i32>::new(platinum) {
        let _ = gp.write(-1);
    }
    0
}
