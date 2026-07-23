use std::sync::{Arc, RwLock};

use tracing::debug;

// pad button bitmasks
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

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct PadState {
    pub buttons: u32,
    pub lx: u8,
    pub ly: u8,
    pub rx: u8,
    pub ry: u8,
    pub l2: u8,
    pub r2: u8,
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
