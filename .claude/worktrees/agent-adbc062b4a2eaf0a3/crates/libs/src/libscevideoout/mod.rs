use crate::context::NativeContext;
use ps4_core::kernel::get_kernel;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::sync::atomic::{AtomicI64, Ordering};
use tracing::info;

/// The `arg` of the most recently submitted flip. The guest's flip loop polls
/// `sceVideoOutGetFlipStatus().flipArg` until it equals the arg it submitted;
/// since HLE presents synchronously, we report the last submitted arg as done.
static LAST_FLIP_ARG: AtomicI64 = AtomicI64::new(0);

#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_OPEN, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutOpen")]
pub fn sce_video_out_open(user_id: i32, bus_type: i32, index: i32, param: u64) -> i32 {
    info!("[VIDEO] sceVideoOutOpen");
    if let Some(k) = get_kernel() {
        k.video_out_open(user_id, bus_type, index, param)
            .unwrap_or(-1)
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_REGISTER_BUFFERS, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutRegisterBuffers")]
pub fn sce_video_out_register_buffers(
    handle: i32,
    start_index: i32,
    ptr: u64,
    count: i32,
    attr_ptr: u64,
) -> i32 {
    info!("[VIDEO] sceVideoOutRegisterBuffers ptr={:#x}", ptr);
    if let Some(k) = get_kernel() {
        k.video_out_register_buffers(handle, start_index, ptr, count, attr_ptr)
            .unwrap_or(-1)
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_SUBMIT_FLIP, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutSubmitFlip")]
pub fn sce_video_out_submit_flip(handle: i32, index: i32, flip_mode: i32, arg: i64) -> i32 {
    LAST_FLIP_ARG.store(arg, Ordering::Relaxed);
    if let Some(k) = get_kernel() {
        k.video_out_submit_flip(handle, index, flip_mode, arg)
            .unwrap_or(-1)
    } else {
        -1
    }
}

#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_SET_FLIP_RATE, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutSetFlipRate")]
pub fn sce_video_out_set_flip_rate(_handle: i32, _rate: i32) -> i32 {
    0 // Always succeed
}

#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_ADD_FLIP_EVENT, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutAddFlipEvent")]
pub fn sce_video_out_add_flip_event(_eq: i32, _handle: i32, _arg: u64) -> i32 {
    // would link the equeue id to the vsync interrupt; WaitEqueue just sleeps for now, so succeed
    0
}

#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_GET_FLIP_STATUS, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutGetFlipStatus")]
pub fn sce_video_out_get_flip_status(_handle: i32, status_ptr: *mut u8) -> i32 {
    if status_ptr.is_null() {
        return 0;
    }
    // OrbisVideoOutFlipStatus is 64 bytes; the guest's flip loop polls flipArg
    // (offset 24) until it equals the arg it submitted. Reporting the last
    // submitted arg lets that loop exit immediately instead of spinning its full
    // timeout every frame (which throttled the whole guest to a crawl).
    let arg = LAST_FLIP_ARG.load(Ordering::Relaxed);
    unsafe {
        std::ptr::write_bytes(status_ptr, 0, 64);
        *(status_ptr.add(24) as *mut i64) = arg;
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_SET_BUFFER_ATTRIBUTE, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutSetBufferAttribute")]
pub fn sce_video_out_set_buffer_attribute(_handle: i32, _attr: u64, _val: u64) -> i32 {
    0
}
