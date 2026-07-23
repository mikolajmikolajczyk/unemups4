pub mod backend;
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
use std::sync::{Arc, RwLock};

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
        let (tx, rx) = unbounded();
        let _ = self.sender.send(GpuCommand::SubmitFlip(handle, index, tx));
        // block until the display loop processes the command
        let _ = rx.recv();
    }

    pub fn register_buffer(&self, ptr: u64, w: u32, h: u32, handle: i32, idx: u32) {
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
        let _ = self
            .sender
            .send(GpuCommand::RegisterBuffer(ptr, w, h, handle, idx));
    }
}

/// Drive the PM4 executor's `SubmitAndFlip` (doc-4 §3) through the *same*
/// block-until-vsync present path `videoout` uses: a `GpuCommand::SubmitFlip`
/// over the display channel, blocking until the display thread has presented. The
/// executor (`ps4-gnm`) names only the Vulkan-free `PresentSink` trait, never this
/// `GpuManager`, so the command processor stays Vulkan-free (decision-4).
impl ps4_core::gpu::PresentSink for GpuManager {
    fn submit_and_flip(&self) {
        // Phase 3 presents the default videoout scanout target; the SubmitAndFlip
        // Gnm ABI's vo-handle/buf-index are not yet threaded through the submit
        // handler, and `submit_flip` presents `CURRENT_TARGET` regardless.
        self.submit_flip(0, 0);
    }

    /// Ship the executor's `BackendCmd` list to the display thread and block until
    /// it is recorded (doc-4 §3): the guest-thread executor stays Vulkan-free and
    /// the display thread (which owns the device) replays it against `AshBackend`.
    /// Phase 3.5 carries the embedded-shader draw. Blocking mirrors the
    /// `SubmitFlip` handshake so the draw is applied before the subsequent flip.
    fn run_command_list(&self, cmds: &[ps4_core::gpu::BackendCmd]) {
        let (tx, rx) = unbounded();
        let _ = self
            .sender
            .send(GpuCommand::RunCommandList(cmds.to_vec(), tx));
        let _ = rx.recv();
    }
}
