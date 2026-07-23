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
use winit::event_loop::{ControlFlow, EventLoop};
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

pub fn run_display_loop(
    rx: Receiver<GpuCommand>,
    guest_memory: Arc<RwLock<Box<dyn ps4_core::memory::VirtualMemoryManager>>>,
    input: InputManager,
) {
    crate::gamepad::spawn(input.clone());

    let event_loop = EventLoop::new().unwrap();
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

    let mut last_present_time = Instant::now();
    let mut fps_counter = 0;
    let mut last_fps_update = Instant::now();

    tracing::debug!("Entering render loop");

    // Aggregate profiler: resolved once. When off (the default), the pacing
    // path below never reads an `Instant` or touches the `PRESENT` atomics.
    let prof = present_profile::enabled();

    let _ = event_loop.run(move |event, target| {
        let backend = match &mut backend {
            Some(b) => b,
            None => return,
        };

        match event {
            Event::AboutToWait => {
                let mut needs_redraw = false;
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        GpuCommand::RegisterBuffer(ptr, w, h, hdl, idx) => {
                            backend.register_buffer(ptr, w, h, hdl, idx);
                        }
                        GpuCommand::SubmitFlip(hdl, idx, signal) => {
                            backend.submit_flip(hdl, idx, signal);
                            needs_redraw = true;
                        }
                        GpuCommand::RunCommandList(cmds, signal) => {
                            // Phase 3.5: replay the executor's embedded
                            // draw into the videoout target (doc-4 §3). Recorded on
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

                // fps + pacing
                let elapsed = last_present_time.elapsed();
                if elapsed < FRAME_DURATION {
                    let sleep = FRAME_DURATION - elapsed;
                    if prof {
                        PRESENT
                            .pace_sleep_ns
                            .fetch_add(sleep.as_nanos() as u64, Ordering::Relaxed);
                    }
                    std::thread::sleep(sleep);
                }
                last_present_time = Instant::now();

                fps_counter += 1;
                if last_fps_update.elapsed() >= Duration::from_secs(1) {
                    window.set_title(&format!("{} - {} FPS", WINDOW_TITLE, fps_counter));
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
