//! `libSceNgs2` HLE — minimal stubs.
//!
//! Ngs2 ("Next Generation Sound 2") is the PS4 audio synthesis/mixing engine:
//! the guest builds a system, adds racks of voices, streams waveform data into
//! voices, and renders the mix into a buffer it then hands to `sceAudioOut`.
//! There is no host 1:1 for the mixer, and a real implementation is a design
//! decision (task-scoped follow-up), so these stubs only make each call succeed
//! with a benign, typed default: system/rack/voice creation hand back non-null
//! opaque handles, render produces nothing (audio still reaches the host sink
//! through the guest's own `sceAudioOut` path), and the voice reports idle.
//!
//! All out-params are written through the range-validated, SMC-tracked write seam
//! (`ps4_core::guest_ptr::GuestPtr`, task-115): a junk pointer fails clean instead of
//! segfaulting the host under the JIT identity map.

use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::ffi::c_void;
use tracing::info;

// Opaque non-null sentinels handed back as Ngs2 handles. The guest only stores
// them and passes them back to other sceNgs2* calls (which we also stub), so any
// distinct non-null value works; it is never dereferenced by us.
const NGS2_SYSTEM_HANDLE: u64 = 0x4E475332_00000001; // "NGS2" tag, system slot
const NGS2_RACK_HANDLE: u64 = 0x4E475332_00000002;
const NGS2_VOICE_HANDLE: u64 = 0x4E475332_00000003;

fn write_handle(out: *mut u64, value: u64) {
    // Range-validated, SMC-tracked write seam (task-115): a junk out-ptr fails clean.
    if let Some(gp) = GuestPtr::<u64>::new(out as u64) {
        let _ = gp.write(value);
    }
}

#[ps4_syscall(id = SyscallId::SCE_NGS2_SYSTEM_CREATE_WITH_ALLOCATOR, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2SystemCreateWithAllocator")]
pub fn sce_ngs2_system_create_with_allocator(
    _option: *const c_void,
    _allocator: *const c_void,
    out_handle: *mut u64,
) -> i32 {
    info!("[NGS2] sceNgs2SystemCreateWithAllocator");
    write_handle(out_handle, NGS2_SYSTEM_HANDLE);
    0
}

#[ps4_syscall(id = SyscallId::SCE_NGS2_RACK_CREATE_WITH_ALLOCATOR, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2RackCreateWithAllocator")]
pub fn sce_ngs2_rack_create_with_allocator(
    _system: u64,
    _rack_id: u32,
    _option: *const c_void,
    _allocator: *const c_void,
    out_handle: *mut u64,
) -> i32 {
    info!("[NGS2] sceNgs2RackCreateWithAllocator");
    write_handle(out_handle, NGS2_RACK_HANDLE);
    0
}

#[ps4_syscall(id = SyscallId::SCE_NGS2_RACK_GET_VOICE_HANDLE, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2RackGetVoiceHandle")]
pub fn sce_ngs2_rack_get_voice_handle(_rack: u64, voice_id: u32, out_handle: *mut u64) -> i32 {
    info!("[NGS2] sceNgs2RackGetVoiceHandle voice_id={}", voice_id);
    write_handle(out_handle, NGS2_VOICE_HANDLE);
    0
}

#[ps4_syscall(id = SyscallId::SCE_NGS2_VOICE_CONTROL, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2VoiceControl")]
pub fn sce_ngs2_voice_control(_voice: u64, _command_list: *const c_void) -> i32 {
    0
}

// sceNgs2VoiceGetStateFlags(voice, outFlags): report 0 (no pending/playing bits)
// so the guest treats every voice as idle/ready and keeps driving the mix.
#[ps4_syscall(id = SyscallId::SCE_NGS2_VOICE_GET_STATE_FLAGS, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2VoiceGetStateFlags")]
pub fn sce_ngs2_voice_get_state_flags(_voice: u64, out_flags: *mut u32) -> i32 {
    if let Some(gp) = GuestPtr::<u32>::new(out_flags as u64) {
        let _ = gp.write(0);
    }
    0
}

// sceNgs2SystemRender(system, bufferInfo, count): render the mix into the guest's
// buffers. With no mixer we leave the buffers untouched (guest-zeroed → silence);
// real sound still flows because the guest submits its own PCM via sceAudioOut.
#[ps4_syscall(id = SyscallId::SCE_NGS2_SYSTEM_RENDER, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2SystemRender")]
pub fn sce_ngs2_system_render(_system: u64, _buffer_info: *const c_void, _count: u32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_NGS2_PARSE_WAVEFORM_DATA, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2ParseWaveformData")]
pub fn sce_ngs2_parse_waveform_data(
    _data: *const c_void,
    _size: u64,
    _out_waveform_info: *mut c_void,
) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// Panning, teardown, and the remaining queries a retail engine reaches for.
// ---------------------------------------------------------------------------

/// `sceNgs2PanInit(...)` — set up a panning work area (speaker layout, matrix scratch).
/// Local bookkeeping for a mixer we do not run; accepted.
#[ps4_syscall(id = SyscallId::SCE_NGS2_PAN_INIT, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2PanInit")]
pub fn sce_ngs2_pan_init() -> i32 {
    0
}

/// `sceNgs2PanGetVolumeMatrix(work, params, count, matrixFormat, outMatrix)` — compute the
/// per-speaker gains for a source position. Refused rather than answered: the out-matrix
/// layout depends on the speaker format we would have to have honoured in `PanInit`, and a
/// wrongly-shaped write lands in a buffer the engine then feeds to its voices. A refusal
/// leaves the caller's matrix as it was, which for an unrendered mix costs nothing.
#[ps4_syscall(id = SyscallId::SCE_NGS2_PAN_GET_VOLUME_MATRIX, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2PanGetVolumeMatrix")]
pub fn sce_ngs2_pan_get_volume_matrix() -> i32 {
    -1
}

/// `sceNgs2SystemDestroy(system)` — tear down a system handle. The handles are sentinels;
/// there is nothing to release, and refusing a teardown only makes an engine log.
#[ps4_syscall(id = SyscallId::SCE_NGS2_SYSTEM_DESTROY, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2SystemDestroy")]
pub fn sce_ngs2_system_destroy() -> i32 {
    0
}

/// `sceNgs2RackDestroy(rack)` — same, for a rack.
#[ps4_syscall(id = SyscallId::SCE_NGS2_RACK_DESTROY, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2RackDestroy")]
pub fn sce_ngs2_rack_destroy() -> i32 {
    0
}

/// `sceNgs2VoiceGetState(voice, outState, size)` — the full voice state block, as opposed to
/// the flags word [`sce_ngs2_voice_get_state_flags`] reports. Refused: the block's layout is
/// version-dependent (the caller passes its `size`), and an engine reads playback position
/// out of it to drive timing — a fabricated position is worse than none.
#[ps4_syscall(id = SyscallId::SCE_NGS2_VOICE_GET_STATE, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2VoiceGetState")]
pub fn sce_ngs2_voice_get_state() -> i32 {
    -1
}

/// `sceNgs2CalcWaveformBlock(...)` — compute block boundaries for a waveform. Refused; we do
/// not parse waveform data into blocks.
#[ps4_syscall(id = SyscallId::SCE_NGS2_CALC_WAVEFORM_BLOCK, lib = crate::libs::LIB_SCE_NGS2, name = "sceNgs2CalcWaveformBlock")]
pub fn sce_ngs2_calc_waveform_block() -> i32 {
    -1
}
