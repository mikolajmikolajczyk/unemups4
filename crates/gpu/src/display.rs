use crossbeam_channel::Receiver;
use ps4_core::gpu::GpuBackend;
use ps4_core::pad::{
    InputManager, PAD_BUTTON_CIRCLE, PAD_BUTTON_CROSS, PAD_BUTTON_DOWN, PAD_BUTTON_L1,
    PAD_BUTTON_L2, PAD_BUTTON_LEFT, PAD_BUTTON_OPTIONS, PAD_BUTTON_R1, PAD_BUTTON_RIGHT,
    PAD_BUTTON_SQUARE, PAD_BUTTON_TRIANGLE, PAD_BUTTON_UP,
};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use winit::dpi::LogicalSize;
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::WindowBuilder;

use crate::backend::{AshBackend, CURRENT_TARGET};
use crate::commands::GpuCommand;
use crate::present_profile::{self, PRESENT};
use crate::vulkan::VulkanContext;

const WINDOW_TITLE: &str = "unemups4";
const RES_W: u32 = 1920;
const RES_H: u32 = 1080;
const TARGET_FPS: u64 = 60;
const FRAME_DURATION: Duration = Duration::from_micros(1_000_000 / TARGET_FPS);

/// Drain and immediately answer every queued command without touching a GPU backend.
/// Used only when Vulkan init failed (no device): the guest thread still boots and blocks
/// on the flip/submit reply channels (`GpuManager::submit_flip` / `run_command_list` ->
/// `rx.recv()`). Answering those channels keeps the blocking guest calls making progress
/// instead of deadlocking forever on a display loop that can never present. Buffer work is
/// dropped (there is nothing to display); the `Sender<()>` reply channels are signalled so
/// each waiting guest call returns.
fn drain_commands_no_backend(rx: &Receiver<GpuCommand>) {
    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            GpuCommand::RegisterBuffer(..) => {}
            GpuCommand::SubmitFlip(_, _, signal) => {
                let _ = signal.send(());
            }
            GpuCommand::RunCommandList(_, signal) => {
                let _ = signal.send(());
            }
        }
    }
}

pub fn run_display_loop(
    rx: Receiver<GpuCommand>,
    guest_memory: Arc<RwLock<Box<dyn ps4_core::memory::VirtualMemoryManager>>>,
    input: InputManager,
) {
    crate::gamepad::spawn(input.clone());

    // winit 0.29 dropped the `WINIT_UNIX_BACKEND` env var, so honor it ourselves: a
    // capture session (RenderDoc can't hook a native Wayland surface) sets
    // `WINIT_UNIX_BACKEND=x11` to get an X11/XWayland window, without forcing X11 on
    // every normal run (Wayland stays the default).
    let mut builder = EventLoopBuilder::<()>::new();
    #[cfg(all(unix, not(target_os = "macos")))]
    if std::env::var("WINIT_UNIX_BACKEND").as_deref() == Ok("x11") {
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_x11();
    }
    let event_loop = builder.build().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);

    let window = WindowBuilder::new()
        .with_title(WINDOW_TITLE)
        .with_inner_size(LogicalSize::new(RES_W as f64, RES_H as f64))
        .build(&event_loop)
        .expect("Failed to create window");

    let mut backend = unsafe {
        match VulkanContext::new(&window) {
            Ok(ctx) => Some(AshBackend::new(ctx, guest_memory)),
            Err(e) => {
                tracing::error!("Failed to initialize Vulkan: {}", e);
                None
            }
        }
    };

    // Real-time frame limiter anchor (task-163). A CPU-light splash otherwise flips as fast
    // as the host can present (hundreds of Hz), which burns host frames the guest never
    // asked for — and under the `fixed-step` clock mode, where virtual time is flips ×
    // 16.667 ms, it also makes guest logic run many times real speed. This anchors the wall
    // time of the last flip so the `SubmitFlip` handler below can pace each flip to ~60 real
    // Hz. It gates FLIPS, not raw presents (which a compositor may drive spuriously).
    let mut last_flip_time = Instant::now();
    let mut fps_counter = 0;
    let mut last_fps_update = Instant::now();
    // Emulated speed, d(virtual)/d(real): the one number that says whether the guest's world
    // is running at the right rate for the frames we are producing.
    let mut speed = ps4_core::clock::SpeedMeter::new();

    tracing::debug!("Entering render loop");

    // Aggregate profiler: resolved once. When off (the default), the pacing
    // path below never reads an `Instant` or touches the `PRESENT` atomics.
    let prof = present_profile::enabled();

    let _ = event_loop.run(move |event, target| {
        let backend = match &mut backend {
            Some(b) => b,
            None => {
                // Vulkan init failed (`VulkanContext::new` returned Err): there is no
                // device to present with, but the guest thread still boots and blocks on
                // the flip/submit handshake (`GpuManager::submit_flip` /
                // `run_command_list` -> `rx.recv()`). Returning on every event without
                // draining `rx` would leave the first blocking guest call waiting on a
                // reply channel forever. Keep answering commands so those calls make
                // progress — discard the (undisplayable) buffer work and immediately
                // signal each reply channel.
                if let Event::AboutToWait = event {
                    drain_commands_no_backend(&rx);
                }
                return;
            }
        };

        match event {
            Event::AboutToWait => {
                let mut needs_redraw = false;
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        GpuCommand::RegisterBuffer(ptr, w, h, pixel_format, hdl, idx) => {
                            backend.register_buffer(ptr, w, h, pixel_format, hdl, idx);
                        }
                        GpuCommand::SubmitFlip(hdl, idx, signal) => {
                            // Real-time frame limiter (task-163). This is the SINGLE
                            // guaranteed once-per-flip choke point: every flip — GNM
                            // (`sceGnmSubmitAndFlipCommandBuffers`) and videoout
                            // (`sceVideoOutSubmitFlip`) — arrives here as exactly one
                            // `SubmitFlip`, and the guest is still blocked (it is signalled
                            // later, inside `present`). Sleeping HERE, before that signal,
                            // paces the guest's flip cadence — and therefore the virtual
                            // clock's 16.667 ms/flip — to real 60 Hz, regardless of how
                            // cheap `present` is or how the compositor drives redraws (the
                            // reason the old `RedrawRequested` sleep was environment-fragile
                            // and let the clock run ~10x on fast hosts). Only ever DELAYS a
                            // flip that is running ahead; a flip that already took longer
                            // than a frame is not slowed (no spiral) because the anchor is
                            // reset to `now`, not advanced by a fixed step.
                            let now = Instant::now();
                            let target = last_flip_time + FRAME_DURATION;
                            if now < target {
                                // Ahead of schedule: sleep the difference. Advance the anchor
                                // to the EXACT target (not to the post-sleep `now`, which
                                // `thread::sleep` overshoots by ~1 ms), so the cadence holds
                                // at 60 Hz without accumulating overshoot drift.
                                let sleep = target - now;
                                if prof {
                                    PRESENT
                                        .pace_sleep_ns
                                        .fetch_add(sleep.as_nanos() as u64, Ordering::Relaxed);
                                }
                                std::thread::sleep(sleep);
                                last_flip_time = target;
                            } else {
                                // A flip that already ran a full frame or longer is not
                                // slowed, and the anchor resets to now so a slow stretch
                                // cannot bank "credit" and then burst (no spiral).
                                last_flip_time = now;
                            }
                            backend.submit_flip(hdl, idx, signal);
                            needs_redraw = true;
                        }
                        GpuCommand::RunCommandList(cmds, signal) => {
                            // Phase 3.5: replay the executor's embedded
                            // draw into the videoout target (doc-2 §3). Recorded on
                            // the display thread that owns the device; the guest
                            // thread blocks on `signal` until it is applied.
                            backend.run_command_list(&cmds);
                            let _ = signal.send(());
                        }
                    }
                }
                if needs_redraw {
                    window.request_redraw();
                }
            }

            Event::WindowEvent {
                event: WindowEvent::RedrawRequested,
                ..
            } => {
                let window_size = window.inner_size();
                if window_size.width == 0 || window_size.height == 0 {
                    backend.signal_vsync();
                    return;
                }

                // Frame span: outer parent for the present phases inside
                // `backend.present`. Cheap callsite check with no span-consuming
                // layer; under Tracy it becomes the per-frame zone with the
                // fence/acquire/fb_copy/record_submit/present children nested inside.
                let _frame = tracing::debug_span!("frame").entered();

                if let Err(e) = backend.present(CURRENT_TARGET) {
                    tracing::error!("present failed: {}", e);
                }

                // Frame pacing is done at the `SubmitFlip` choke point above (task-163), not
                // here: a present may be driven spuriously by the compositor without a flip,
                // and pacing raw presents let the actual flip rate (which drives the virtual
                // clock) outrun 60 Hz on fast hosts.
                fps_counter += 1;
                if last_fps_update.elapsed() >= Duration::from_secs(1) {
                    window.set_title(&format!(
                        "{} - {} FPS - flip {} - speed {:.0}%",
                        WINDOW_TITLE,
                        fps_counter,
                        ps4_core::clock::flip_count(),
                        speed.sample(),
                    ));
                    fps_counter = 0;
                    last_fps_update = Instant::now();
                }

                // NB: the guest was already signalled inside `backend.present` --
                // after the staging memcpy (staging path) or after queue_submit
                // (zero-copy path) -- so present + pacing above run in
                // parallel with the guest's next frame. Do NOT signal again here.
            }

            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                unsafe {
                    backend.ctx().device.device_wait_idle().unwrap();
                }
                target.exit();
            }
            Event::WindowEvent {
                event: WindowEvent::KeyboardInput { event, .. },
                ..
            } => {
                let pressed = event.state == winit::event::ElementState::Pressed;
                if let PhysicalKey::Code(keycode) = event.physical_key {
                    // On-demand GPU state snapshot (task-185). F10 = the next complete frame,
                    // F9 = a burst of `UNEMUPS4_SNAPSHOT_FRAMES` frames.
                    //
                    // This handler runs on the DISPLAY THREAD, which must never acquire
                    // `driver()` (task-43/task-66): the guest thread holds that lock across
                    // `exec.run(...)`, which blocks on this thread's channel, so a lock here
                    // deadlocks instantly and silently. So the request touches no GPU state at
                    // all — it bumps one atomic counter that the gnm executor drains at its
                    // next frame boundary, on the submit thread. Nothing here blocks, locks,
                    // allocates, or reads back.
                    //
                    // `!event.repeat` so holding the key does not queue hundreds of captures.
                    if pressed && !event.repeat {
                        match keycode {
                            KeyCode::F10 => ps4_core::snapshot::request(1),
                            KeyCode::F9 => {
                                ps4_core::snapshot::request(ps4_core::snapshot::burst_frames())
                            }
                            _ => {}
                        }
                    }
                    // The ps4doom shim maps DS4 buttons to Doom keys, so the keyboard
                    // targets the same DS4 bits: Ctrl/Enter->CROSS (fire), Space->CIRCLE
                    // (use), Shift->SQUARE (run), Enter also as menu-select via TRIANGLE,
                    // Esc->OPTIONS (menu), Q/E->L1/R1 (strafe), Tab->L2 (automap). Enter
                    // drives both CROSS and TRIANGLE so a single key fires in-game and
                    // confirms in menus.
                    match keycode {
                        KeyCode::ControlLeft | KeyCode::ControlRight => {
                            input.set_button(PAD_BUTTON_CROSS, pressed)
                        }
                        KeyCode::Enter => {
                            input.set_button(PAD_BUTTON_CROSS, pressed);
                            input.set_button(PAD_BUTTON_TRIANGLE, pressed);
                        }
                        KeyCode::Escape => input.set_button(PAD_BUTTON_OPTIONS, pressed),
                        KeyCode::Space => input.set_button(PAD_BUTTON_CIRCLE, pressed),
                        KeyCode::ShiftLeft | KeyCode::ShiftRight => {
                            input.set_button(PAD_BUTTON_SQUARE, pressed)
                        }
                        KeyCode::KeyQ => input.set_button(PAD_BUTTON_L1, pressed),
                        KeyCode::KeyE => input.set_button(PAD_BUTTON_R1, pressed),
                        KeyCode::Tab => input.set_button(PAD_BUTTON_L2, pressed),
                        KeyCode::ArrowUp => input.set_button(PAD_BUTTON_UP, pressed),
                        KeyCode::ArrowDown => input.set_button(PAD_BUTTON_DOWN, pressed),
                        KeyCode::ArrowLeft => input.set_button(PAD_BUTTON_LEFT, pressed),
                        KeyCode::ArrowRight => input.set_button(PAD_BUTTON_RIGHT, pressed),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    /// When Vulkan init fails the display loop has no backend, but the guest thread still
    /// blocks on the flip/submit reply channels. `drain_commands_no_backend` must answer
    /// each blocking command's reply channel so those guest calls do not deadlock (the
    /// CONFIRMED deadlock this fix targets), and must drop buffer work without blocking.
    #[test]
    fn drain_commands_no_backend_answers_blocking_signals() {
        let (tx, rx) = unbounded();

        let (flip_tx, flip_rx) = unbounded();
        tx.send(GpuCommand::SubmitFlip(0, 0, flip_tx)).unwrap();

        let (cmd_tx, cmd_rx) = unbounded();
        tx.send(GpuCommand::RunCommandList(Vec::new(), cmd_tx))
            .unwrap();

        // A register-buffer between the two blocking commands must be consumed, not left
        // to wedge the drain loop.
        tx.send(GpuCommand::RegisterBuffer(0x1000, RES_W, RES_H, 0, 0, 0))
            .unwrap();

        drain_commands_no_backend(&rx);

        // Both blocking guest calls (`submit_flip` / `run_command_list`) would now unblock.
        assert!(flip_rx.try_recv().is_ok(), "SubmitFlip reply not signalled");
        assert!(
            cmd_rx.try_recv().is_ok(),
            "RunCommandList reply not signalled"
        );
        // The queue is fully drained.
        assert!(rx.try_recv().is_err());
    }
}
