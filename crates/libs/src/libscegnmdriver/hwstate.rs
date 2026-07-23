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

/// `sceGnmDrawInitToDefaultContextState(cmdbuf, size)` — writes the default *context*
/// state PM4 preamble (the CONTEXT-bank register defaults, as opposed to the HW-state
/// family above) into the cmdbuf. Return the dword count consumed so the caller advances
/// its write cursor. No PM4 written yet (the executor derives draw state from the guest's
/// own SET_*_REG stream, doc-2 §5).
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INIT_TO_DEFAULT_CONTEXT_STATE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawInitToDefaultContextState"
)]
pub fn sce_gnm_draw_init_to_default_context_state(_cmdbuf: u64, size: u32) -> u32 {
    size
}

/// `sceGnmDrawInitToDefaultContextState400(cmdbuf, size)` — SDK-400 variant of the
/// context-state preamble builder. Same size-returning stub.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DRAW_INIT_TO_DEFAULT_CONTEXT_STATE400,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDrawInitToDefaultContextState400"
)]
pub fn sce_gnm_draw_init_to_default_context_state400(_cmdbuf: u64, size: u32) -> u32 {
    size
}

/// `sceGnmDispatchInitDefaultHardwareState(cmdbuf, size)` — the compute-queue counterpart
/// of `sceGnmDrawInitDefaultHardwareState`: writes the default HW-state preamble for a
/// dispatch (compute) command buffer. Return the dword count consumed. No PM4 written yet.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_DISPATCH_INIT_DEFAULT_HARDWARE_STATE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmDispatchInitDefaultHardwareState"
)]
pub fn sce_gnm_dispatch_init_default_hardware_state(_cmdbuf: u64, size: u32) -> u32 {
    size
}

/// `sceGnmSetVgtControl(cmdbuf, primgroup_size, ...)` — writes a `VGT_PRIMGROUP` /
/// tessellation-distribution PM4 packet into the cmdbuf. This tunes the vertex-grouping
/// hardware; a software rasterizer ignores it. Return the dword count the packet would
/// consume (1 — the packet is a single SET_UCONFIG_REG run the executor already tolerates
/// as an unhandled register write). No PM4 emitted; the value is bookkeeping.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_SET_VGT_CONTROL,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmSetVgtControl"
)]
pub fn sce_gnm_set_vgt_control(_cmdbuf: u64, _primgroup_size: u32, _mode: u32) -> u32 {
    0
}

/// `sceGnmResetVgtControl(cmdbuf)` — restores the default VGT control. No-op success; the
/// software rasterizer has no VGT (see [`sce_gnm_set_vgt_control`]).
#[ps4_syscall(
    id = SyscallId::SCE_GNM_RESET_VGT_CONTROL,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmResetVgtControl"
)]
pub fn sce_gnm_reset_vgt_control(_cmdbuf: u64) -> u32 {
    0
}

// ── Command-buffer validation getters (doc-6 Entry 1 §3) ──────────────────────────────
//
// The GNM validator is a debug-only correctness checker the SDK runs over a command
// buffer before submit. Our executor validates as it decodes (bad packets are skipped,
// never fatal), so the validator surface is a set of constant-success stubs: "no
// diagnostics, validation disabled/passed". A title that gates a submit on the validator
// returning OK proceeds.

/// `sceGnmValidateOnSubmitEnabled()` — is on-submit validation enabled? Return 0
/// (disabled) — we validate during decode, not via this path.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_VALIDATE_ON_SUBMIT_ENABLED,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmValidateOnSubmitEnabled"
)]
pub fn sce_gnm_validate_on_submit_enabled() -> i32 {
    0
}

/// `sceGnmValidateDisableDiagnostics(...)` — disable validator diagnostics. No-op success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_VALIDATE_DISABLE_DIAGNOSTICS,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmValidateDisableDiagnostics"
)]
pub fn sce_gnm_validate_disable_diagnostics(_arg: u64) -> i32 {
    0
}

/// `sceGnmValidateDisableDiagnostics2(...)` — the second overload. No-op success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_VALIDATE_DISABLE_DIAGNOSTICS2,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmValidateDisableDiagnostics2"
)]
pub fn sce_gnm_validate_disable_diagnostics2(_arg0: u64, _arg1: u64) -> i32 {
    0
}

/// `sceGnmValidateResetState()` — reset the validator's accumulated state. No-op success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_VALIDATE_RESET_STATE,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmValidateResetState"
)]
pub fn sce_gnm_validate_reset_state() -> i32 {
    0
}

/// `sceGnmValidateGetVersion()` — validator version. Return 0 (no validator).
#[ps4_syscall(
    id = SyscallId::SCE_GNM_VALIDATE_GET_VERSION,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmValidateGetVersion"
)]
pub fn sce_gnm_validate_get_version() -> i32 {
    0
}

/// `sceGnmValidateGetDiagnosticInfo(...)` — return no diagnostics. Success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_VALIDATE_GET_DIAGNOSTIC_INFO,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmValidateGetDiagnosticInfo"
)]
pub fn sce_gnm_validate_get_diagnostic_info(_arg0: u64, _arg1: u64, _arg2: u64) -> i32 {
    0
}

/// `sceGnmValidateGetDiagnostics(...)` — return no diagnostics. Success.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_VALIDATE_GET_DIAGNOSTICS,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmValidateGetDiagnostics"
)]
pub fn sce_gnm_validate_get_diagnostics(_arg0: u64, _arg1: u64) -> i32 {
    0
}

/// `sceGnmGetDebugTimestamp()` — a monotonic GPU debug timestamp. Return 0; nothing gates
/// correctness on it.
#[ps4_syscall(
    id = SyscallId::SCE_GNM_GET_DEBUG_TIMESTAMP,
    lib = crate::libs::LIB_SCE_GNM_DRIVER,
    name = "sceGnmGetDebugTimestamp"
)]
pub fn sce_gnm_get_debug_timestamp() -> i64 {
    0
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
