use std::sync::{Arc, RwLock};

use tracing::debug;

// DualShock 4 button bitmasks, as returned to the guest in the low 32 bits of
// `OrbisPadData.buttons` (offset 0). Values are the `OrbisPadButton` enum in the
// OpenOrbis SDK header `include/orbis/_types/pad.h` (`ORBIS_PAD_BUTTON_*`); pinned
// by `pad_buttons_match_orbis_oracle` below. The guest reads them via
// scePadRead/scePadReadState.
pub const PAD_BUTTON_L3: u32 = 0x000002;
pub const PAD_BUTTON_R3: u32 = 0x000004;
pub const PAD_BUTTON_OPTIONS: u32 = 0x000008;
pub const PAD_BUTTON_UP: u32 = 0x000010;
pub const PAD_BUTTON_RIGHT: u32 = 0x000020;
pub const PAD_BUTTON_DOWN: u32 = 0x000040;
pub const PAD_BUTTON_LEFT: u32 = 0x000080;
pub const PAD_BUTTON_L2: u32 = 0x000100;
pub const PAD_BUTTON_R2: u32 = 0x000200;
pub const PAD_BUTTON_L1: u32 = 0x000400;
pub const PAD_BUTTON_R1: u32 = 0x000800;
pub const PAD_BUTTON_TRIANGLE: u32 = 0x001000;
pub const PAD_BUTTON_CIRCLE: u32 = 0x002000;
pub const PAD_BUTTON_CROSS: u32 = 0x004000;
pub const PAD_BUTTON_SQUARE: u32 = 0x008000;

/// The controller sample we hand back to the guest. Field order mirrors the head
/// of the OpenOrbis SDK `OrbisPadData` struct (`include/orbis/_types/pad.h`):
/// `buttons` (u32), `leftStick{x,y}`, `rightStick{x,y}`, `analogButtons{l2,r2}` —
/// i.e. `lx`=leftStick.x, `ly`=leftStick.y, `rx`=rightStick.x, `ry`=rightStick.y.
/// The sticks and triggers are `uint8_t` there. `scePadReadState`/`scePadRead`
/// (crates/libs libscepad) serialise these fields to the same byte offsets the guest
/// expects (buttons 0..4, lx 4, ly 5, rx 6, ry 7).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PadState {
    pub buttons: u32,
    pub lx: u8,
    pub ly: u8,
    pub rx: u8,
    pub ry: u8,
    pub l2: u8,
    pub r2: u8,
}

/// Analog sticks default to 0x80 — the neutral centre of the `uint8_t` axis
/// (`OrbisPadData` stick fields are `uint8_t`, per OpenOrbis `_types/pad.h`).
/// The 0x80 midpoint and its polarity (0x00 = full up/left, 0xFF = full
/// down/right) are our empirically-fixed convention (task-192), not a value the
/// oracle header states. A zeroed default reads to the guest as a stick pinned
/// fully up-left every frame, which dragged Celeste's menu selection back to the
/// top item constantly. `set_button` only touches `buttons`, so an untouched
/// stick stays centred here.
impl Default for PadState {
    fn default() -> Self {
        Self {
            buttons: 0,
            lx: 0x80,
            ly: 0x80,
            rx: 0x80,
            ry: 0x80,
            l2: 0,
            r2: 0,
        }
    }
}

#[derive(Clone)]
pub struct InputManager {
    pub state: Arc<RwLock<PadState>>,
}

impl Default for InputManager {
    fn default() -> Self {
        Self::new()
    }
}

impl InputManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(PadState::default())),
        }
    }

    pub fn set_button(&self, button: u32, pressed: bool) {
        debug!(
            "Button state changed: button={}, pressed={}",
            button, pressed
        );
        let mut s = self.state.write().unwrap();
        if pressed {
            s.buttons |= button;
        } else {
            s.buttons &= !button;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins each `PAD_BUTTON_*` mask to its published DualShock 4 value. The
    /// right-hand literals are the `OrbisPadButton` enum in the OpenOrbis SDK header
    /// `include/orbis/_types/pad.h`; this test fails if ours drift from those values.
    #[test]
    fn pad_buttons_match_orbis_oracle() {
        // (our const, ORBIS_PAD_BUTTON_* value from OpenOrbis _types/pad.h).
        let oracle: [(u32, u32); 15] = [
            (PAD_BUTTON_L3, 0x0002),
            (PAD_BUTTON_R3, 0x0004),
            (PAD_BUTTON_OPTIONS, 0x0008),
            (PAD_BUTTON_UP, 0x0010),
            (PAD_BUTTON_RIGHT, 0x0020),
            (PAD_BUTTON_DOWN, 0x0040),
            (PAD_BUTTON_LEFT, 0x0080),
            (PAD_BUTTON_L2, 0x0100),
            (PAD_BUTTON_R2, 0x0200),
            (PAD_BUTTON_L1, 0x0400),
            (PAD_BUTTON_R1, 0x0800),
            (PAD_BUTTON_TRIANGLE, 0x1000),
            (PAD_BUTTON_CIRCLE, 0x2000),
            (PAD_BUTTON_CROSS, 0x4000),
            (PAD_BUTTON_SQUARE, 0x8000),
        ];
        for (ours, orbis) in oracle {
            assert_eq!(ours, orbis, "pad button {ours:#06X} != ORBIS {orbis:#06X}");
        }
    }

    /// The default sample is a released pad with both sticks centred at the
    /// `uint8_t` midpoint 0x80 (see [`PadState`]'s doc-comment: sticks are `uint8_t`
    /// per OpenOrbis `_types/pad.h`; 0x80-neutral is our convention, task-192).
    #[test]
    fn default_pad_state_is_released_and_centred() {
        let s = PadState::default();
        assert_eq!(s.buttons, 0);
        assert_eq!((s.lx, s.ly, s.rx, s.ry), (0x80, 0x80, 0x80, 0x80));
        assert_eq!((s.l2, s.r2), (0, 0));
    }
}
