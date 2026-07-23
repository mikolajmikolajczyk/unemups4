use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestSlice;
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
pub fn sce_video_out_set_flip_rate(_handle: i32, rate: i32) -> i32 {
    // The rate the guest ASKS for is the only statement it makes about its intended
    // frame cadence (0 = 60 Hz, 1 = 30 Hz, 2 = 20 Hz). We do not honour it yet, but the
    // virtual clock advances a fixed 16.67 ms per flip (`ps4_core::clock`), so a guest
    // that requested 30 Hz runs at half speed. Log it rather than discarding it silently.
    tracing::info!("sceVideoOutSetFlipRate(rate={rate}) — accepted, not yet honoured");
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
    // Build the 64-byte status locally (flipArg i64 @offset 24), then write it in one
    // range-validated, SMC-tracked shot (task-115): a bad/near-arena-top `status_ptr` fails
    // clean instead of overrunning host memory.
    let mut status = [0u8; 64];
    status[24..32].copy_from_slice(&arg.to_le_bytes());
    if let Some(gs) = GuestSlice::<u8>::new(status_ptr as u64, 64) {
        let _ = gs.write_slice(&status);
    }
    0
}

/// `sceVideoOutSetBufferAttribute(SceVideoOutBufferAttribute* attribute, uint32 pixelFormat,
/// uint32 tilingMode, uint32 aspectRatio, uint32 width, uint32 height, uint32 pixelPitch)`.
///
/// A WRITER: it fills the guest's `*attribute` so a later `sceVideoOutRegisterBuffers(...,
/// attribute)` reads the real scanout geometry (`read_videoout_attr`, width @+12/height @+16).
/// The vendored SDK header (`data/oo_sdk/include/orbis/_types/video.h`) fixes the 40-byte
/// layout: `{ i32 format@0; i32 tmode@4; i32 aspect@8; u32 width@12; u32 height@16;
/// u32 pixelPitch@20; u64 reserved[2]@24 }`. The no-op stub left the guest's stack-allocated
/// attr as garbage, so `read_videoout_attr` saw width=1/height=3 → a degenerate 1x3 display
/// buffer (the white-on-black cause).
///
/// ABI (SysV, arg3=r10 for the SYSCALL RCX-clobber): attribute=arg0(rdi), pixelFormat=arg1,
/// tilingMode=arg2, aspectRatio=arg3, width=arg4, height=arg5; pixelPitch is the 7th arg,
/// stack-passed (`syscall_stack_arg(6)`). Written through the bounded, SMC-tracked
/// [`GuestSlice`] seam (task-115/task-138), never a raw guest store.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_SET_BUFFER_ATTRIBUTE, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutSetBufferAttribute")]
pub fn sce_video_out_set_buffer_attribute(
    attribute: u64,
    pixel_format: u32,
    tiling_mode: u32,
    aspect_ratio: u32,
    width: u32,
    height: u32,
) -> i32 {
    // 7th arg (pixelPitch) is stack-passed on SysV; args 0..5 are in registers.
    let pixel_pitch = ps4_cpu::syscall_stack_arg(6) as u32;
    info!(
        "[VIDEO] sceVideoOutSetBufferAttribute attr={:#x} fmt={:#x} tmode={} aspect={} {}x{} pitch={}",
        attribute, pixel_format, tiling_mode, aspect_ratio, width, height, pixel_pitch
    );
    // Build the 40-byte struct locally (zeroed reserved[2] tail so a stack-allocated attr
    // can't leak garbage), then write it in one bounded, range-validated shot.
    let buf = encode_buffer_attribute(
        pixel_format,
        tiling_mode,
        aspect_ratio,
        width,
        height,
        pixel_pitch,
    );
    if let Some(gs) = GuestSlice::<u8>::new(attribute, 40) {
        let _ = gs.write_slice(&buf);
    }
    0
}

/// Encode an `OrbisVideoOutBufferAttribute` (SDK layout: format@0, tmode@4, aspect@8,
/// width@12, height@16, pixelPitch@20, reserved[2]@24) into a 40-byte buffer. Split out
/// so the offset layout is unit-testable without a syscall context, and matches exactly
/// what `read_videoout_attr` (bridge.rs) reads back at +12/+16.
fn encode_buffer_attribute(
    pixel_format: u32,
    tiling_mode: u32,
    aspect_ratio: u32,
    width: u32,
    height: u32,
    pixel_pitch: u32,
) -> [u8; 40] {
    let mut buf = [0u8; 40];
    buf[0..4].copy_from_slice(&(pixel_format as i32).to_le_bytes());
    buf[4..8].copy_from_slice(&(tiling_mode as i32).to_le_bytes());
    buf[8..12].copy_from_slice(&(aspect_ratio as i32).to_le_bytes());
    buf[12..16].copy_from_slice(&width.to_le_bytes());
    buf[16..20].copy_from_slice(&height.to_le_bytes());
    buf[20..24].copy_from_slice(&pixel_pitch.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::encode_buffer_attribute;

    /// Mirror of `read_videoout_attr` (bridge.rs): width @+12, height @+16 (SDK layout).
    /// Reasoned from the SDK offsets, not captured from the production reader.
    fn read_wh(attr: &[u8]) -> (u32, u32) {
        let w = u32::from_le_bytes(attr[12..16].try_into().unwrap());
        let h = u32::from_le_bytes(attr[16..20].try_into().unwrap());
        (w, h)
    }

    #[test]
    fn set_attribute_round_trips_1080p() {
        // Regression guard for the 1x3 white-on-black bug: the no-op writer left the attr
        // garbage so register_buffers read width=1/height=3. With the real writer, the
        // reader at +12/+16 sees the geometry we wrote back verbatim.
        let attr = encode_buffer_attribute(0x8000_2200, 1, 0, 1920, 1080, 1920);
        assert_eq!(read_wh(&attr), (1920, 1080));
        assert_eq!(
            u32::from_le_bytes(attr[0..4].try_into().unwrap()),
            0x8000_2200
        );
        assert_eq!(u32::from_le_bytes(attr[20..24].try_into().unwrap()), 1920);
        assert_eq!(&attr[24..40], &[0u8; 16], "reserved[2] is zeroed");
    }

    #[test]
    fn set_attribute_round_trips_720p() {
        let attr = encode_buffer_attribute(0, 0, 0, 1280, 720, 1280);
        assert_eq!(read_wh(&attr), (1280, 720));
    }
}

/// The scanout geometry we actually present, mirroring `ps4_gpu::display`'s `RES_W`/`RES_H`.
/// Duplicated rather than imported because `ps4-libs` sits below `ps4-gpu` in the crate
/// graph; if the display loop's resolution ever changes, these must change with it, or a
/// title will lay its UI out for a screen it is not being given.
const SCANOUT_WIDTH: u32 = 1920;
const SCANOUT_HEIGHT: u32 = 1080;

/// `sceVideoOutAddVblankEvent(eq, handle, udata)` — ask for a scanout-vblank event on an
/// event queue, the way `sceVideoOutAddFlipEvent` asks for a flip event.
///
/// Registration succeeds and no vblank is ever posted. That is safe here only because of
/// how [`crate::libkernel::events::sce_kernel_wait_equeue`] behaves: a wait with nothing
/// pending sleeps ~16 ms and returns zero events rather than blocking. A title waiting for
/// vblank therefore still advances at roughly display cadence — it is paced by the timeout
/// instead of by the event. If a title ever *requires* the event itself (a frame counter
/// derived from vblank deliveries, say), this is where a real 60 Hz signal has to be wired
/// in, and the tell will be a frame counter that never moves.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_ADD_VBLANK_EVENT, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutAddVblankEvent")]
pub fn sce_video_out_add_vblank_event(_eq: i32, _handle: i32, _udata: u64) -> i32 {
    0
}

/// `sceVideoOutGetEventCount(const SceKernelEvent *ev)` — how many vblanks the reported
/// event stands for. Answers 1: one wait, one display period elapsed.
///
/// Zero would be the other candidate and is worse. Engines use this to advance a vblank
/// counter and to detect dropped frames; a steady zero reads as "time is not passing on the
/// display", which is exactly the input that makes a frame pacer either spin or conclude
/// the output died.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_GET_EVENT_COUNT, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutGetEventCount")]
pub fn sce_video_out_get_event_count(_ev: u64) -> i32 {
    1
}

/// `sceVideoOutGetResolutionStatus(handle, OrbisVideoOutResolutionStatus *status)` — the
/// display the title is rendering for. Filled from the geometry we really scan out, so a
/// title that sizes its render targets or its UI safe-area from this call gets the window
/// it will actually be shown in.
///
/// `refreshRate` is left zero on purpose. It is an enumerated value on hardware, not a rate
/// in Hz, and we have not verified the enumeration; writing `60` into an enum field would be
/// a fabricated constant that reads as verified. Zero is the unset reading. If a title turns
/// out to need it, the symptom will be frame pacing computed from a bad refresh, and this
/// comment is the place to start.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_GET_RESOLUTION_STATUS, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutGetResolutionStatus")]
pub fn sce_video_out_get_resolution_status(_handle: i32, status: u64) -> i32 {
    let buf = encode_resolution_status(SCANOUT_WIDTH, SCANOUT_HEIGHT);
    if let Some(gs) = GuestSlice::<u8>::new(status, 48) {
        let _ = gs.write_slice(&buf);
    }
    0
}

/// Encode an `OrbisVideoOutResolutionStatus` (SDK layout, `_types/video.h`: width@0,
/// height@4, paneWidth@8, paneHeight@12, refreshRate@16, screenSize@24, flags@28,
/// reserved0@30, reserved1[3]@32). Split out so the offsets are testable without a syscall
/// context. The pane is the whole screen — we do not letterbox.
fn encode_resolution_status(width: u32, height: u32) -> [u8; 48] {
    let mut buf = [0u8; 48];
    buf[0..4].copy_from_slice(&width.to_le_bytes());
    buf[4..8].copy_from_slice(&height.to_le_bytes());
    buf[8..12].copy_from_slice(&width.to_le_bytes());
    buf[12..16].copy_from_slice(&height.to_le_bytes());
    buf
}

/// `sceVideoOutUnregisterBuffers(handle, attributeIndex)` — drop a registered buffer set.
/// Succeeds: the display loop reads whatever the *current* registration points at, and a
/// title only unregisters when it is about to register something else.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_UNREGISTER_BUFFERS, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutUnregisterBuffers")]
pub fn sce_video_out_unregister_buffers(_handle: i32, _attribute_index: i32) -> i32 {
    0
}

/// `sceVideoOutClose(handle)` — release the video-out port. Succeeds. The display loop owns
/// the window for the life of the process, so there is nothing to tear down; a title only
/// reaches this on shutdown.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_CLOSE, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutClose")]
pub fn sce_video_out_close(_handle: i32) -> i32 {
    0
}

/// `sceVideoOutConfigureOutputMode_(...)` — set the output mode (resolution, HDR, deep
/// colour) for the port. Accepted and ignored: we present one fixed 1920x1080 SDR surface,
/// and the mode a title asks for does not change what the display loop scans out.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_CONFIGURE_OUTPUT_MODE, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutConfigureOutputMode_")]
pub fn sce_video_out_configure_output_mode() -> i32 {
    0
}

/// `sceVideoOutModeSetAny_(...)` — the "any acceptable mode" form of the same request.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_MODE_SET_ANY, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutModeSetAny_")]
pub fn sce_video_out_mode_set_any() -> i32 {
    0
}

/// `sceVideoOutSetWindowModeMargins(...)` — reserve margins for the system's window mode.
/// Accepted; nothing overlays our surface.
#[ps4_syscall(id = SyscallId::SCE_VIDEO_OUT_SET_WINDOW_MODE_MARGINS, lib = crate::libs::LIB_SCE_VIDEO_OUT, name = "sceVideoOutSetWindowModeMargins")]
pub fn sce_video_out_set_window_mode_margins() -> i32 {
    0
}
