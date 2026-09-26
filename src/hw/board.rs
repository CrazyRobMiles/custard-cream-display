//! Waveshare ESP32-S3-Touch-LCD-7 wiring.
//!
//! Taken from Waveshare's board definition and proven by the `panel-demo`
//! port in the ESP32-Panel project. The GPIO numbers are also used directly
//! in `bin/main.rs`, where the peripherals are handed out.
//!
//! | Function        | Pin(s)                                              |
//! |-----------------|-----------------------------------------------------|
//! | RGB data B3..B7 | GPIO14, 38, 18, 17, 10                              |
//! | RGB data G2..G7 | GPIO39, 0, 45, 48, 47, 21                           |
//! | RGB data R3..R7 | GPIO1, 2, 42, 41, 40                                |
//! | HSYNC/VSYNC     | GPIO46 / GPIO3                                      |
//! | DE / PCLK       | GPIO5 / GPIO7                                       |
//! | I2C SDA/SCL     | GPIO8 / GPIO9 (CH422G, GT911)                       |
//! | Touch INT       | GPIO4                                               |
//!
//! The panel size and timings come from `settings.toml`
//! ([`crate::config::display`]).

/// I2C address of the touch controller.
pub const GT911_ADDR: u8 = 0x5D;

/// CH422G expander outputs (EXIO numbers are bit positions).
pub mod exio {
    pub const TP_RST: u8 = 1;
    pub const LCD_BL: u8 = 2;
    pub const LCD_RST: u8 = 3;
    pub const SD_CS: u8 = 4;
}
