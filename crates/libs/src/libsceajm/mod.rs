//! `libSceAjm` HLE — minimal stubs.
//!
//! Ajm ("Audio Job Manager") is the PS4 batched audio-codec service: the guest
//! registers a context, registers codec modules (AT9/MP3/AAC), creates decoder
//! instances, then submits *batches* of decode jobs and waits on them. Games
//! that use FMOD (e.g. this managed-runtime title) initialise Ajm during audio
//! bring-up before any frame is drawn.
//!
//! There is no host 1:1 for the job manager and real decode is a task-scoped
//! follow-up, so these stubs only make the lifecycle succeed with benign, typed
//! defaults: initialize/create hand back non-null opaque handles, register/
//! destroy/finalize succeed, and query calls report a safe zero. Actual PCM
//! still reaches the host sink through the guest's own `sceAudioOut` path once
//! decode is wired; until then decode simply produces nothing.
//!
//! All out-params are written through the range-validated, SMC-tracked write seam
//! (`ps4_core::guest_ptr::GuestPtr`, task-115): a junk pointer fails clean instead of
//! segfaulting the host under the JIT identity map.

use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use tracing::info;

// Opaque non-null sentinels handed back as Ajm handles. The guest only stores
// them and passes them back to other sceAjm* calls (which we also stub), so any
// distinct non-null value works; it is never dereferenced by us. "AJM" tag.
const AJM_CONTEXT_ID: u32 = 0x0A_4A_4D_01; // context slot
const AJM_INSTANCE_ID: u32 = 0x0A_4A_4D_02; // instance slot

fn write_u32(out: *mut u32, value: u32) {
    // Range-validated, SMC-tracked write seam (task-115): a junk out-ptr fails clean.
    if let Some(gp) = GuestPtr::<u32>::new(out as u64) {
        let _ = gp.write(value);
    }
}

/// `sceAjmInitialize(iReserved, pContextId)` — create the Ajm context. Hands
/// back a non-null context id and succeeds.
#[ps4_syscall(id = SyscallId::SCE_AJM_INITIALIZE, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmInitialize")]
pub fn sce_ajm_initialize(_reserved: i64, out_context: *mut u32) -> i32 {
    info!("[AJM] sceAjmInitialize");
    write_u32(out_context, AJM_CONTEXT_ID);
    0
}

/// `sceAjmFinalize(context)` — tear down the context. Succeeds.
#[ps4_syscall(id = SyscallId::SCE_AJM_FINALIZE, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmFinalize")]
pub fn sce_ajm_finalize(_context: u32) -> i32 {
    info!("[AJM] sceAjmFinalize");
    0
}

/// `sceAjmModuleRegister(context, codec, reserved)` — register a codec module
/// (AT9/MP3/AAC). Succeeds without loading anything.
#[ps4_syscall(id = SyscallId::SCE_AJM_MODULE_REGISTER, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmModuleRegister")]
pub fn sce_ajm_module_register(_context: u32, codec: u32, _reserved: i64) -> i32 {
    info!("[AJM] sceAjmModuleRegister codec={}", codec);
    0
}

/// `sceAjmModuleUnregister(context, codec)` — unregister a codec module.
#[ps4_syscall(id = SyscallId::SCE_AJM_MODULE_UNREGISTER, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmModuleUnregister")]
pub fn sce_ajm_module_unregister(_context: u32, _codec: u32) -> i32 {
    0
}

/// `sceAjmInstanceCreate(context, codec, flags, pInstance)` — create a decoder
/// instance. Hands back a non-null instance id and succeeds.
#[ps4_syscall(id = SyscallId::SCE_AJM_INSTANCE_CREATE, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmInstanceCreate")]
pub fn sce_ajm_instance_create(
    _context: u32,
    codec: u32,
    _flags: u64,
    out_instance: *mut u32,
) -> i32 {
    info!("[AJM] sceAjmInstanceCreate codec={}", codec);
    write_u32(out_instance, AJM_INSTANCE_ID);
    0
}

/// `sceAjmInstanceDestroy(context, instance)` — destroy a decoder instance.
#[ps4_syscall(id = SyscallId::SCE_AJM_INSTANCE_DESTROY, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmInstanceDestroy")]
pub fn sce_ajm_instance_destroy(_context: u32, _instance: u32) -> i32 {
    0
}

/// `sceAjmInstanceExtend(context, instance, flags)` — extend an instance's
/// capabilities. Succeeds.
#[ps4_syscall(id = SyscallId::SCE_AJM_INSTANCE_EXTEND, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmInstanceExtend")]
pub fn sce_ajm_instance_extend(_context: u32, _instance: u32, _flags: u64) -> i32 {
    0
}

/// `sceAjmInstanceSwitch(context, instance, flags)` — switch an instance's
/// codec parameters. Succeeds.
#[ps4_syscall(id = SyscallId::SCE_AJM_INSTANCE_SWITCH, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmInstanceSwitch")]
pub fn sce_ajm_instance_switch(_context: u32, _instance: u32, _flags: u64) -> i32 {
    0
}

/// `sceAjmInstanceGetSize(...)` — query the byte size Ajm needs for an instance.
/// Status-only stub (0 = OK).
#[ps4_syscall(id = SyscallId::SCE_AJM_INSTANCE_GET_SIZE, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmInstanceGetSize")]
pub fn sce_ajm_instance_get_size(_context: u32, _codec: u32, _flags: u64) -> i32 {
    0
}

/// `sceAjmInstanceCodecType(context, instance)` — report the codec bound to an
/// instance. Reports 0.
#[ps4_syscall(id = SyscallId::SCE_AJM_INSTANCE_CODEC_TYPE, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmInstanceCodecType")]
pub fn sce_ajm_instance_codec_type(_context: u32, _instance: u32) -> i32 {
    0
}

/// `sceAjmInstanceAvailable(...)` — report whether an instance is available.
/// Reports 0 (available/ready).
#[ps4_syscall(id = SyscallId::SCE_AJM_INSTANCE_AVAILABLE, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmInstanceAvailable")]
pub fn sce_ajm_instance_available(_context: u32, _instance: u32) -> i32 {
    0
}

/// `sceAjmMemoryRegister(context, pMemory, size)` — register a guest memory
/// region for Ajm to use. Succeeds without taking ownership.
#[ps4_syscall(id = SyscallId::SCE_AJM_MEMORY_REGISTER, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmMemoryRegister")]
pub fn sce_ajm_memory_register(_context: u32, _memory: u64, _size: u64) -> i32 {
    info!("[AJM] sceAjmMemoryRegister");
    0
}

/// `sceAjmMemoryUnregister(context, pMemory)` — unregister a guest region.
#[ps4_syscall(id = SyscallId::SCE_AJM_MEMORY_UNREGISTER, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmMemoryUnregister")]
pub fn sce_ajm_memory_unregister(_context: u32, _memory: u64) -> i32 {
    0
}

/// `sceAjmGetFailedInstance(...)` — report which instance in a failed batch
/// errored. Reports 0.
#[ps4_syscall(id = SyscallId::SCE_AJM_GET_FAILED_INSTANCE, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmGetFailedInstance")]
pub fn sce_ajm_get_failed_instance(_context: u32, _batch: u64) -> i32 {
    0
}

/// `sceAjmStrError(error)` — map an error code to a string. Null (no message);
/// callers null-check or print "(null)".
#[ps4_syscall(id = SyscallId::SCE_AJM_STR_ERROR, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmStrError")]
pub fn sce_ajm_str_error(_error: i32) -> u64 {
    0
}

/// `sceAjmDecAt9ParseConfigData(...)` — parse an ATRAC9 config blob. Status-only
/// stub (0 = OK); does not populate the (unknown) out struct.
#[ps4_syscall(id = SyscallId::SCE_AJM_DEC_AT9_PARSE_CONFIG_DATA, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmDecAt9ParseConfigData")]
pub fn sce_ajm_dec_at9_parse_config_data(_config: u64, _out: u64) -> i32 {
    0
}

/// `sceAjmDecMp3ParseFrame(...)` — parse an MP3 frame header. Status-only stub
/// (0 = OK).
#[ps4_syscall(id = SyscallId::SCE_AJM_DEC_MP3_PARSE_FRAME, lib = crate::libs::LIB_SCE_AJM, name = "sceAjmDecMp3ParseFrame")]
pub fn sce_ajm_dec_mp3_parse_frame(
    _frame: u64,
    _size: u32,
    _channels: u32,
    _flags: u32,
    _out: u64,
) -> i32 {
    0
}
