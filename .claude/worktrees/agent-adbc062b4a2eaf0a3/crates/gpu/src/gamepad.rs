use std::thread;
use std::time::Duration;

use gilrs::{Axis, Button, EventType, Gilrs};
use ps4_core::pad::{
    InputManager, PAD_BUTTON_CIRCLE, PAD_BUTTON_CROSS, PAD_BUTTON_DOWN, PAD_BUTTON_L1,
    PAD_BUTTON_L2, PAD_BUTTON_L3, PAD_BUTTON_LEFT, PAD_BUTTON_OPTIONS, PAD_BUTTON_R1,
    PAD_BUTTON_R2, PAD_BUTTON_R3, PAD_BUTTON_RIGHT, PAD_BUTTON_SQUARE, PAD_BUTTON_TRIANGLE,
    PAD_BUTTON_UP,
};

// gilrs is event-driven but non-blocking; poll on a modest cadence so an idle
// controller costs ~nothing while inputs still land within a frame.
const POLL_INTERVAL: Duration = Duration::from_millis(4);
// Doom is digital: past this the left stick counts as a full D-pad press.
const STICK_DEADZONE: f32 = 0.5;

fn button_mask(button: Button) -> Option<u32> {
    Some(match button {
        Button::South => PAD_BUTTON_CROSS,
        Button::East => PAD_BUTTON_CIRCLE,
        Button::West => PAD_BUTTON_SQUARE,
        Button::North => PAD_BUTTON_TRIANGLE,
        Button::Start => PAD_BUTTON_OPTIONS,
        Button::DPadUp => PAD_BUTTON_UP,
        Button::DPadDown => PAD_BUTTON_DOWN,
        Button::DPadLeft => PAD_BUTTON_LEFT,
        Button::DPadRight => PAD_BUTTON_RIGHT,
        Button::LeftTrigger => PAD_BUTTON_L1,
        Button::RightTrigger => PAD_BUTTON_R1,
        Button::LeftTrigger2 => PAD_BUTTON_L2,
        Button::RightTrigger2 => PAD_BUTTON_R2,
        Button::LeftThumb => PAD_BUTTON_L3,
        Button::RightThumb => PAD_BUTTON_R3,
        _ => return None,
    })
}

fn poll_loop(mut gilrs: Gilrs, input: InputManager) {
    loop {
        while let Some(event) = gilrs.next_event() {
            match event.event {
                EventType::ButtonPressed(b, _) => {
                    if let Some(mask) = button_mask(b) {
                        input.set_button(mask, true);
                    }
                }
                EventType::ButtonReleased(b, _) => {
                    if let Some(mask) = button_mask(b) {
                        input.set_button(mask, false);
                    }
                }
                EventType::AxisChanged(axis, value, _) => match axis {
                    Axis::LeftStickX => {
                        input.set_button(PAD_BUTTON_LEFT, value < -STICK_DEADZONE);
                        input.set_button(PAD_BUTTON_RIGHT, value > STICK_DEADZONE);
                    }
                    Axis::LeftStickY => {
                        input.set_button(PAD_BUTTON_UP, value > STICK_DEADZONE);
                        input.set_button(PAD_BUTTON_DOWN, value < -STICK_DEADZONE);
                    }
                    _ => {}
                },
                EventType::Disconnected => {
                    for mask in [
                        PAD_BUTTON_LEFT,
                        PAD_BUTTON_RIGHT,
                        PAD_BUTTON_UP,
                        PAD_BUTTON_DOWN,
                    ] {
                        input.set_button(mask, false);
                    }
                }
                _ => {}
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Spawn the physical-gamepad poller. The [`Gilrs`] instance is owned by a
/// dedicated thread that writes the shared [`InputManager`], decoupled from the
/// winit event loop and the softgpu present so it never stalls a frame.
///
/// Graceful degradation: if no gamepad backend is available (headless devShell,
/// no udev/evdev), log once and return — the keyboard path in `display.rs` still
/// drives the guest.
///
/// macOS note: gilrs on macOS drives input through IOKit, which expects to be
/// pumped from the app's main run loop. A dedicated background thread is fine on
/// Linux (evdev); a future macOS port should poll gilrs on the winit main thread
/// instead of here.
pub fn spawn(input: InputManager) {
    match Gilrs::new() {
        Ok(gilrs) => {
            thread::Builder::new()
                .name("gamepad".into())
                .spawn(move || poll_loop(gilrs, input))
                .ok();
        }
        Err(e) => {
            tracing::debug!("gamepad input unavailable: {e}");
        }
    }
}
