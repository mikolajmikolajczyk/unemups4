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

// The four direction bits are driven by BOTH the physical D-pad and the left stick. Each
// source keeps its own mask (see `poll_loop`) and the two are OR'd, so a stick recenter
// clears only the stick's contribution and never a concurrently-held D-pad direction.
const DIR_MASK: u32 = PAD_BUTTON_LEFT | PAD_BUTTON_RIGHT | PAD_BUTTON_UP | PAD_BUTTON_DOWN;

// Every button this poller maps. Cleared as a set on `Disconnected`, which arrives with no
// matching `ButtonReleased` for a held button, so a face/shoulder button (or L3/R3) would
// otherwise stay latched in `PadState` after the pad vanishes.
const ALL_BUTTONS: u32 = PAD_BUTTON_CROSS
    | PAD_BUTTON_CIRCLE
    | PAD_BUTTON_SQUARE
    | PAD_BUTTON_TRIANGLE
    | PAD_BUTTON_OPTIONS
    | PAD_BUTTON_UP
    | PAD_BUTTON_DOWN
    | PAD_BUTTON_LEFT
    | PAD_BUTTON_RIGHT
    | PAD_BUTTON_L1
    | PAD_BUTTON_R1
    | PAD_BUTTON_L2
    | PAD_BUTTON_R2
    | PAD_BUTTON_L3
    | PAD_BUTTON_R3;

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

/// Write the four direction bits as the OR of the D-pad and stick contributions, so
/// neither source can clear a direction the other is still holding.
fn apply_dirs(input: &InputManager, dpad_dirs: u32, stick_dirs: u32) {
    let combined = dpad_dirs | stick_dirs;
    for mask in [
        PAD_BUTTON_LEFT,
        PAD_BUTTON_RIGHT,
        PAD_BUTTON_UP,
        PAD_BUTTON_DOWN,
    ] {
        input.set_button(mask, combined & mask != 0);
    }
}

fn poll_loop(mut gilrs: Gilrs, input: InputManager) {
    // The direction bits from the physical D-pad and from the analog left stick, tracked
    // separately and OR'd through `apply_dirs`. Last-writer-wins on the shared bit would let
    // an axis recenter (AxisChanged ~0) clear a concurrently-held D-pad direction.
    let mut dpad_dirs: u32 = 0;
    let mut stick_dirs: u32 = 0;
    loop {
        while let Some(event) = gilrs.next_event() {
            match event.event {
                EventType::ButtonPressed(b, _) => {
                    if let Some(mask) = button_mask(b) {
                        if mask & DIR_MASK != 0 {
                            dpad_dirs |= mask;
                            apply_dirs(&input, dpad_dirs, stick_dirs);
                        } else {
                            input.set_button(mask, true);
                        }
                    }
                }
                EventType::ButtonReleased(b, _) => {
                    if let Some(mask) = button_mask(b) {
                        if mask & DIR_MASK != 0 {
                            dpad_dirs &= !mask;
                            apply_dirs(&input, dpad_dirs, stick_dirs);
                        } else {
                            input.set_button(mask, false);
                        }
                    }
                }
                EventType::AxisChanged(axis, value, _) => match axis {
                    Axis::LeftStickX => {
                        stick_dirs &= !(PAD_BUTTON_LEFT | PAD_BUTTON_RIGHT);
                        if value < -STICK_DEADZONE {
                            stick_dirs |= PAD_BUTTON_LEFT;
                        }
                        if value > STICK_DEADZONE {
                            stick_dirs |= PAD_BUTTON_RIGHT;
                        }
                        apply_dirs(&input, dpad_dirs, stick_dirs);
                    }
                    Axis::LeftStickY => {
                        stick_dirs &= !(PAD_BUTTON_UP | PAD_BUTTON_DOWN);
                        if value > STICK_DEADZONE {
                            stick_dirs |= PAD_BUTTON_UP;
                        }
                        if value < -STICK_DEADZONE {
                            stick_dirs |= PAD_BUTTON_DOWN;
                        }
                        apply_dirs(&input, dpad_dirs, stick_dirs);
                    }
                    _ => {}
                },
                EventType::Disconnected => {
                    // A held button gets no matching ButtonReleased when the pad vanishes;
                    // clear the whole mapped button word (not just the four directions) so
                    // nothing stays latched for the guest.
                    dpad_dirs = 0;
                    stick_dirs = 0;
                    input.set_button(ALL_BUTTONS, false);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn buttons(input: &InputManager) -> u32 {
        input.state.read().unwrap().buttons
    }

    // A D-pad RIGHT held while the left stick recenters must keep PAD_BUTTON_RIGHT set: the
    // stick's zero contribution only clears the stick mask, and the OR preserves the D-pad's.
    #[test]
    fn stick_recenter_keeps_held_dpad_direction() {
        let input = InputManager::new();
        let dpad_dirs = PAD_BUTTON_RIGHT; // D-pad Right physically held.
        let stick_dirs = 0; // Stick recentered inside the deadzone.
        apply_dirs(&input, dpad_dirs, stick_dirs);
        assert_eq!(buttons(&input) & PAD_BUTTON_RIGHT, PAD_BUTTON_RIGHT);
    }

    // Stick and D-pad contributions to the same direction OR together, and releasing one
    // source leaves the other's bit set.
    #[test]
    fn dpad_and_stick_or_together() {
        let input = InputManager::new();
        apply_dirs(&input, PAD_BUTTON_LEFT, PAD_BUTTON_LEFT);
        assert_eq!(buttons(&input) & PAD_BUTTON_LEFT, PAD_BUTTON_LEFT);
        // Stick releases LEFT; D-pad still holds it.
        apply_dirs(&input, PAD_BUTTON_LEFT, 0);
        assert_eq!(buttons(&input) & PAD_BUTTON_LEFT, PAD_BUTTON_LEFT);
        // D-pad releases too; the bit clears.
        apply_dirs(&input, 0, 0);
        assert_eq!(buttons(&input) & PAD_BUTTON_LEFT, 0);
    }
}
