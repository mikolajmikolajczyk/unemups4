use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestSlice;
use ps4_core::kernel::{HandleKind, get_kernel, handle_alloc, handle_free, handle_resolve};
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

#[ps4_syscall(id = SyscallId::SCE_PAD_INIT, lib = crate::libs::LIB_SCE_PAD, name = "scePadInit")]
pub fn sce_pad_init() -> i32 {
    0 // Success
}

#[ps4_syscall(id = SyscallId::SCE_PAD_OPEN, lib = crate::libs::LIB_SCE_PAD, name = "scePadOpen")]
pub fn sce_pad_open(_user_id: i32, _type: i32, _index: i32, _param: u64) -> i32 {
    // Hand back a kind-tagged arena handle (task-115) instead of the old fixed `1`, so a later
    // `scePadClose`/`scePadReadState` can validate the handle's kind + liveness. The handle
    // value is opaque to the guest (`pad_get_state` ignores it), so any distinct positive id
    // works; `unwrap_or(1)` keeps the old behaviour if the table can't allocate (never in
    // practice, id space is 2^24).
    handle_alloc(HandleKind::Pad).unwrap_or(1)
}

#[ps4_syscall(id = SyscallId::SCE_PAD_CLOSE, lib = crate::libs::LIB_SCE_PAD, name = "scePadClose")]
pub fn sce_pad_close(handle: i32) -> i32 {
    // Retire the handle. A stale/foreign handle is a detectable no-op; close still reports
    // success, which is what the guest expects.
    handle_free(handle, Some(HandleKind::Pad));
    0
}

/// Upper bound on the number of `OrbisPadData` records `scePadRead` materialises in one call.
/// `count` reaches the syscall fully guest-controlled (only `count <= 0` is rejected at the top),
/// so it is clamped to this small defensive ceiling before driving the write loop: a corrupted or
/// hostile value up to `i32::MAX` would otherwise spin the loop for billions of iterations inside
/// the syscall, freezing the guest thread (most iterations past the arena top just fail
/// `GuestSlice::new` and do nothing, yet the loop still runs to `count`). A per-frame pad poll
/// drains only a handful of queued samples, so no legitimate caller reads fewer records than before.
const PAD_READ_MAX_SAMPLES: i32 = 64;

/// Clamp a guest-supplied `scePadRead` sample count to the defensive ceiling above. Callers pass a
/// value already known to be `> 0`; the result is in `1..=PAD_READ_MAX_SAMPLES`.
fn pad_read_sample_count(count: i32) -> i32 {
    count.min(PAD_READ_MAX_SAMPLES)
}

/// A pad handle is usable if it resolves as a live `Pad` handle. To stay compatible with the
/// legacy fixed `1` (which some callers may still pass, and which `scePadOpen` falls back to
/// if the arena couldn't allocate), a non-tagged value — one that isn't a `Pad`-tagged arena
/// handle at all — is treated leniently and allowed; only a `Pad`-tagged handle that is no
/// longer live is rejected.
fn pad_handle_ok(handle: i32) -> bool {
    if ps4_core::kernel::handle_kind(handle) == Some(HandleKind::Pad) {
        handle_resolve(handle, Some(HandleKind::Pad))
    } else {
        true
    }
}

#[ps4_syscall(id = SyscallId::SCE_PAD_READ_STATE, lib = crate::libs::LIB_SCE_PAD, name = "scePadReadState")]
pub fn sce_pad_read_state(handle: i32, data_ptr: *mut u8) -> i32 {
    if data_ptr.is_null() {
        return -1;
    }
    if !pad_handle_ok(handle) {
        return -1; // stale/freed Pad handle
    }

    if let Some(k) = get_kernel() {
        let state = k.pad_get_state(handle);
        if state.buttons != 0 {
            tracing::debug!("scePadReadState: buttons={:#06x}", state.buttons);
        }

        // Build the 96-byte OrbisPadData locally, then write it in one range-validated,
        // SMC-tracked shot (task-115): a bad/near-arena-top `data_ptr` fails clean instead of
        // overrunning host memory.
        let mut data = [0u8; 96];
        data[0..4].copy_from_slice(&state.buttons.to_le_bytes());
        // sticks
        data[4] = state.lx;
        data[5] = state.ly;
        data[6] = state.rx;
        data[7] = state.ry;
        data[8] = state.l2;
        data[9] = state.r2;
        // connected flag at offset 76 so guest sees a controller
        data[76] = 1;
        if let Some(gs) = GuestSlice::<u8>::new(data_ptr as u64, 96) {
            let _ = gs.write_slice(&data);
        }
        0
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_PAD_READ, lib = crate::libs::LIB_SCE_PAD, name = "scePadRead")]
pub fn sce_pad_read(handle: i32, data_ptr: *mut u8, count: i32) -> i32 {
    if data_ptr.is_null() || count <= 0 {
        return -1;
    }
    if !pad_handle_ok(handle) {
        return -1; // stale/freed Pad handle
    }

    if let Some(k) = get_kernel() {
        let state = k.pad_get_state(handle);
        let struct_size = 96; // OrbisPadData size

        // Build one OrbisPadData record locally (identical for every polled sample), then write
        // each of the `count` records in a range-validated, SMC-tracked shot (task-115): a
        // bad/near-arena-top `data_ptr` fails clean instead of overrunning host memory.
        let mut data = [0u8; 96];
        data[0..4].copy_from_slice(&state.buttons.to_le_bytes());
        // sticks
        data[4] = state.lx;
        data[5] = state.ly;
        data[6] = state.rx;
        data[7] = state.ry;
        // triggers
        data[8] = state.l2;
        data[9] = state.r2;
        // connected byte; offset depends on SDK struct layout, set all known candidates
        data[12] = 1;
        data[76] = 1; // OpenOrbis
        data[88] = 1; // official SDK
        // `count` is fully guest-controlled; clamp before iterating so a huge/hostile value can't
        // spin this loop for billions of iterations (see `pad_read_sample_count`).
        let n = pad_read_sample_count(count);
        for i in 0..n {
            let current = data_ptr as u64 + (i as u64) * struct_size as u64;
            if let Some(gs) = GuestSlice::<u8>::new(current, struct_size) {
                let _ = gs.write_slice(&data);
            }
        }
        0
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_PAD_GET_HANDLE, lib = crate::libs::LIB_SCE_PAD, name = "scePadGetHandle")]
pub fn sce_pad_get_handle(_user_id: i32, _type: i32, _index: i32) -> i32 {
    // `scePadGetHandle` returns the handle for an already-opened port; hand back a kind-tagged
    // arena handle (task-115) so it validates like `scePadOpen`'s.
    handle_alloc(HandleKind::Pad).unwrap_or(1)
}

// Haptics + DS4 light-bar setters. There is no rumble motor or light bar on a native host,
// so these are benign no-ops returning SCE_OK (0). They are INPUT-TRIGGERED (a game only
// rumbles / recolors the pad on a gameplay event), so a headless run never reaches them —
// Celeste calls `scePadSetVibration` the first time the player acts in-game (task-170); a
// missing symbol here aborted the process mid-play. Light-bar recolor is per-level in Celeste.
#[ps4_syscall(id = SyscallId::SCE_PAD_SET_VIBRATION, lib = crate::libs::LIB_SCE_PAD, name = "scePadSetVibration")]
pub fn sce_pad_set_vibration(_handle: i32, _param: u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PAD_SET_LIGHT_BAR, lib = crate::libs::LIB_SCE_PAD, name = "scePadSetLightBar")]
pub fn sce_pad_set_light_bar(_handle: i32, _param: u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_PAD_RESET_LIGHT_BAR, lib = crate::libs::LIB_SCE_PAD, name = "scePadResetLightBar")]
pub fn sce_pad_reset_light_bar(_handle: i32) -> i32 {
    0
}

#[cfg(test)]
mod tests {
    use super::{PAD_READ_MAX_SAMPLES, pad_read_sample_count};

    #[test]
    fn pad_read_count_is_bounded() {
        // Small valid counts pass through unchanged (correct-input behaviour preserved).
        assert_eq!(pad_read_sample_count(1), 1);
        assert_eq!(
            pad_read_sample_count(PAD_READ_MAX_SAMPLES),
            PAD_READ_MAX_SAMPLES
        );
        // Anything above the ceiling — including a hostile i32::MAX — is clamped, so scePadRead
        // never spins billions of iterations on a guest-controlled count.
        assert_eq!(
            pad_read_sample_count(PAD_READ_MAX_SAMPLES + 1),
            PAD_READ_MAX_SAMPLES
        );
        assert_eq!(pad_read_sample_count(1_000_000), PAD_READ_MAX_SAMPLES);
        assert_eq!(pad_read_sample_count(i32::MAX), PAD_READ_MAX_SAMPLES);
    }
}
