//! RGB panel configuration for the LCD_CAM DPI driver.

use esp_hal::gpio::Level;
use esp_hal::lcd_cam::lcd::dpi::{Config, Format, FrameTiming};
use esp_hal::lcd_cam::lcd::{ClockMode, Phase, Polarity};
use esp_hal::time::Rate;

use crate::config::display::*;

pub fn dpi_config() -> Config {
    Config::default()
        .with_frequency(Rate::from_hz(PIXEL_CLOCK_HZ))
        // pclk_active_neg = 1 in Waveshare's ESP-IDF configuration.
        .with_clock_mode(ClockMode {
            polarity: Polarity::IdleLow,
            phase: Phase::ShiftHigh,
        })
        .with_format(Format {
            enable_2byte_mode: true,
            ..Default::default()
        })
        .with_timing(FrameTiming {
            horizontal_active_width: WIDTH,
            horizontal_total_width: HSYNC_PULSE + HSYNC_BACK_PORCH + WIDTH + HSYNC_FRONT_PORCH,
            horizontal_blank_front_porch: HSYNC_PULSE + HSYNC_BACK_PORCH,
            vertical_active_height: HEIGHT,
            vertical_total_height: VSYNC_PULSE + VSYNC_BACK_PORCH + HEIGHT + VSYNC_FRONT_PORCH,
            vertical_blank_front_porch: VSYNC_PULSE + VSYNC_BACK_PORCH,
            hsync_width: HSYNC_PULSE,
            vsync_width: VSYNC_PULSE,
            hsync_position: 0,
        })
        .with_vsync_idle_level(Level::High)
        .with_hsync_idle_level(Level::High)
        .with_de_idle_level(Level::Low)
        .with_disable_black_region(false)
}
