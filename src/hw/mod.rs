//! Display hardware. Only the Waveshare ESP32-S3-Touch-LCD-7 is supported
//! so far (`[display] type` in `settings.toml`); its bring-up sequence is in
//! `bin/main.rs`.

pub mod board;
pub mod ch422g;
pub mod display;
pub mod framebuffer;
pub mod gt911;

use crate::config::{display as panel, panel_to_logical, touch};

/// The I2C bus shared by the I/O expander and the touch controller.
pub type I2cBus = esp_hal::i2c::master::I2c<'static, esp_hal::Blocking>;

/// Converts a raw touch position to the viewer's coordinates, applying the
/// `[touch]` corrections and then the display rotation. Positions outside
/// the panel are clamped to its edge.
pub fn touch_to_logical(x: u16, y: u16) -> (usize, usize) {
    let (mut x, mut y) = (x as usize, y as usize);
    if touch::SWAP_XY {
        core::mem::swap(&mut x, &mut y);
    }
    let x = x.min(panel::WIDTH - 1);
    let y = y.min(panel::HEIGHT - 1);
    let x = if touch::INVERT_X { panel::WIDTH - 1 - x } else { x };
    let y = if touch::INVERT_Y { panel::HEIGHT - 1 - y } else { y };
    panel_to_logical(x, y)
}
