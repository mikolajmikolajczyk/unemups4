pub mod backend;
pub mod buffer_pool;
pub mod commands;
pub mod display;
pub mod gamepad;
pub mod present_profile;
mod vulkan;

pub use backend::AshBackend;
pub use commands::GpuCommand;
use crossbeam_channel::{Receiver, Sender, unbounded};
pub use display::run_display_loop;
use ps4_core::gpu::{DisplayBuffer, DisplayBufferSource};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Guest-thread-readable mirror of the videoout-registered framebuffers, keyed by the
/// `(handle, index)` the display side uses. The draw path's render-target derivation
/// (`ps4-gnm`) consults this through the [`DisplayBufferSource`] seam to recognize when a
/// `CB_COLOR0_BASE` aliases a real display buffer; the display thread keeps its own copy
/// for present. Registrations happen on the kernel thread (`sceVideoOutRegisterBuffers`)
/// before draws submit, so a plain `RwLock` mirror suffices.
#[derive(Default)]
struct DisplayBufferRegistry {
    buffers: RwLock<HashMap<(i32, u32), DisplayBuffer>>,
}

impl DisplayBufferSource for DisplayBufferRegistry {
    fn lookup(&self, base: u64) -> Option<DisplayBuffer> {
        let map = self.buffers.read().ok()?;
        map.values().find(|b| b.base == base).copied()
    }
}

pub struct GpuManager {
    sender: Sender<GpuCommand>,
    display_buffers: Arc<DisplayBufferRegistry>,
}

impl GpuManager {
    pub fn new() -> (Self, Receiver<GpuCommand>) {
        let (tx, rx) = unbounded();
        (
            Self {
                sender: tx,
                display_buffers: Arc::new(DisplayBufferRegistry::default()),
            },
            rx,
        )
    }

    /// The [`DisplayBufferSource`] backing the videoout registrations, for the boot code
    /// to wire into the process-global seam (`ps4_core::gpu::register_display_buffers`) so
    /// the draw path can map a color target to its display buffer.
    pub fn display_buffer_source(&self) -> Arc<dyn DisplayBufferSource> {
        self.display_buffers.clone()
    }

    pub fn send_command(&self, cmd: GpuCommand) {
        let _ = self.sender.send(cmd);
    }
    pub fn submit_flip(&self, handle: i32, index: u32) {
        let _span = tracing::debug_span!("gpu_flip_wait").entered();
        let t = present_profile::enabled().then(Instant::now);
        let (tx, rx) = unbounded();
        let _ = self.sender.send(GpuCommand::SubmitFlip(handle, index, tx));
        // block until the display loop processes the command
        let _ = rx.recv();
        if let Some(t) = t {
            present_profile::SUBMIT
                .guest_flip_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        // One presented flip = one guest frame boundary: it re-arms the virtual clock's
        // per-frame max-delta clamp (and, under `fixed-step`, is the whole time base). This
        // inherent fn is the single choke point both flip flavors funnel through
        // (`VideoOutSink::submit_flip` for `sceVideoOutSubmitFlip`,
        // `PresentSink::submit_and_flip` for the GNM submit), so the clock sees exactly one
        // frame boundary per presented frame.
        ps4_core::clock::advance_frame();
    }

    pub fn register_buffer(
        &self,
        ptr: u64,
        w: u32,
        h: u32,
        // The guest scanout pixelFormat, threaded on to the present shader so it can
        // R↔B-swap BGRA scanout formats (task-154 residual #2).
        pixel_format: u32,
        handle: i32,
        idx: u32,
    ) {
        // Mirror synchronously so the draw path (guest thread) sees the registration
        // before the guest submits a draw against it; the display thread also gets the
        // command for its own present-side map.
        if let Ok(mut map) = self.display_buffers.buffers.write() {
            map.insert(
                (handle, idx),
                DisplayBuffer {
                    base: ptr,
                    width: w,
                    height: h,
                },
            );
        }
        let _ = self.sender.send(GpuCommand::RegisterBuffer(
            ptr,
            w,
            h,
            pixel_format,
            handle,
            idx,
        ));
    }
}

/// Drive the PM4 executor's `SubmitAndFlip` (doc-2 §3) through the *same*
/// block-until-vsync present path `videoout` uses: a `GpuCommand::SubmitFlip`
/// over the display channel, blocking until the display thread has presented. The
/// executor (`ps4-gnm`) names only the Vulkan-free `PresentSink` trait, never this
/// `GpuManager`, so the command processor stays Vulkan-free (decision-4).
impl ps4_core::gpu::PresentSink for GpuManager {
    fn submit_and_flip(&self, vo_handle: i32, buf_idx: u32) {
        // Present the buffer the guest's `sceGnmSubmitAndFlipCommandBuffers` named
        // (its vo-handle/buf-index, decoded from the submit ABI). `submit_flip` sets the
        // display thread's `current_target = (vo_handle, buf_idx)`, so the present path
        // blits *that* registration's scanout buffer — for a double-buffered title this
        // is the just-rendered frame, not a fixed index-0 buffer.
        self.submit_flip(vo_handle, buf_idx);
    }

    /// Ship the executor's `BackendCmd` list to the display thread and block until
    /// it is recorded (doc-2 §3): the guest-thread executor stays Vulkan-free and
    /// the display thread (which owns the device) replays it against `AshBackend`.
    /// Phase 3.5 carries the embedded-shader draw. Blocking mirrors the
    /// `SubmitFlip` handshake so the draw is applied before the subsequent flip.
    fn run_command_list(&self, cmds: &[ps4_core::gpu::BackendCmd]) {
        let _span = tracing::debug_span!("gpu_submit_wait").entered();
        let t = present_profile::enabled().then(Instant::now);
        let (tx, rx) = unbounded();
        let _ = self
            .sender
            .send(GpuCommand::RunCommandList(cmds.to_vec(), tx));
        let _ = rx.recv();
        if let Some(t) = t {
            present_profile::SUBMIT
                .guest_submit_calls
                .fetch_add(1, Ordering::Relaxed);
            present_profile::SUBMIT
                .guest_submit_ns
                .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
}

/// The videoout present seam (doc-2 §3): `sceVideoOutRegisterBuffers`/`sceVideoOutSubmitFlip`
/// reach the display channel through this trait so `ps4-kernel` need not depend on `ps4-gpu`.
/// It just forwards to the inherent [`GpuManager::register_buffer`]/[`GpuManager::submit_flip`]
/// (the same block-until-vsync path videoout already used), threading the real framebuffer
/// geometry the kernel parsed from the guest attribute instead of a hardcoded resolution.
impl ps4_core::videoout::VideoOutSink for GpuManager {
    fn register_buffer(
        &self,
        base: u64,
        attr: ps4_core::videoout::VideoOutBufferAttribute,
        handle: i32,
        index: u32,
    ) {
        self.register_buffer(
            base,
            attr.width,
            attr.height,
            attr.pixel_format,
            handle,
            index,
        );
    }

    fn submit_flip(&self, handle: i32, index: u32) {
        self.submit_flip(handle, index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIX 1 outcome: a double-buffered title registers BOTH scanout buffers, so both
    /// `(handle, index)` land in the mirror the draw/present path consults, each resolving
    /// to its own base. Before the fix only `list[0]` was read, so Celeste's second buffer
    /// (0x982c48000, the scene target at index 1) was never registered.
    #[test]
    fn register_both_buffers_maps_each_index() {
        let (gpu, _rx) = GpuManager::new();
        gpu.register_buffer(0x981670000, 1920, 1080, 0x8000_0000, 0, 0);
        gpu.register_buffer(0x982c48000, 1920, 1080, 0x8000_0000, 0, 1);

        let src = gpu.display_buffer_source();
        assert_eq!(src.lookup(0x981670000).expect("index 0").base, 0x981670000);
        assert_eq!(src.lookup(0x982c48000).expect("index 1").base, 0x982c48000);

        let map = gpu.display_buffers.buffers.read().unwrap();
        assert_eq!(map.get(&(0, 0)).unwrap().base, 0x981670000);
        assert_eq!(map.get(&(0, 1)).unwrap().base, 0x982c48000);
        assert_eq!(map.get(&(0, 1)).unwrap().width, 1920);
    }
}
