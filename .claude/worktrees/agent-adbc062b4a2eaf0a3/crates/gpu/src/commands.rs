use crossbeam_channel::Sender;
use ps4_core::gpu::BackendCmd;

pub enum GpuCommand {
    /// Registers a memory buffer for video output.
    /// Args: (GuestPtr, Width, Height, Handle, BufferIndex)
    RegisterBuffer(u64, u32, u32, i32, u32),

    /// Submits a buffer to be displayed on the screen.
    /// Args: (Handle, BufferIndex, VSyncSignal)
    /// The VSyncSignal is a channel that MUST be triggered when the frame is presented.
    SubmitFlip(i32, u32, Sender<()>),

    /// Replay a PM4-executor-emitted [`BackendCmd`] list against the ash backend on
    /// the display thread (doc-4 §3: the guest-thread executor ships one data list per
    /// submit; the display thread owns the device). Carries the pipeline-create /
    /// bind-by-id / draw commands (`CreatePipeline` + `BindPipeline` + `DrawAuto`) into
    /// the videoout target. The `Sender<()>` is signalled once the list has been
    /// recorded so the guest-thread `run_command_list` call blocks until it is applied,
    /// matching the `SubmitFlip` handshake.
    RunCommandList(Vec<BackendCmd>, Sender<()>),
}

pub struct DisplayBuffer {
    pub guest_ptr: u64,
    pub width: u32,
    pub height: u32,
}
