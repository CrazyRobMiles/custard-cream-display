//! Pictures: decoding a JPEG from the server into a screenful of RGB565
//! pixels, and mixing two screenfuls for a crossfade.
//!
//! A [`Picture`] is stored in panel order (already rotated, scaled and
//! dithered), so showing or fading it is a straight pass over memory.
//!
//! This crate is compiled without the FPU (see Cargo.toml), so the pixel
//! loops use fixed-point integer arithmetic only.

use alloc::vec;
use alloc::vec::Vec;

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;
use zune_jpeg::errors::DecodeErrors;

use crate::config::display::{HEIGHT, WIDTH};
use crate::config::{Scale, logical_size, panel_to_logical, slideshow};

pub const PIXELS: usize = WIDTH * HEIGHT;

/// The number of distinct steps in a crossfade. RGB565 has 5 bits of red
/// and blue, so 32 steps is as fine as a fade can be.
pub const FADE_STEPS: u32 = 32;

/// Refuse to decode anything bigger than this (the server sends at most
/// 1600 pixels a side).
const MAX_SOURCE_SIZE: usize = 2048;

/// A screenful of RGB565 pixels in panel order, in PSRAM.
pub struct Picture {
    pixels: Vec<u16>,
}

impl Picture {
    /// A screen filled with one colour.
    pub fn blank(colour: u16) -> Self {
        Self {
            pixels: vec![colour; PIXELS],
        }
    }

    /// Decodes a JPEG and fits it to the screen, as set by
    /// `[slideshow] scale` and `[display] rotation`.
    ///
    /// `rgb` is scratch space for the full-size decoded image. Keep it and
    /// pass it in each time: a fresh buffer of 1.4 MB or more for every
    /// picture fragments PSRAM until one can no longer be found.
    pub fn from_jpeg(jpeg: &[u8], rgb: &mut Vec<u8>) -> Result<Self, DecodeErrors> {
        let options = DecoderOptions::default()
            .jpeg_set_out_colorspace(ColorSpace::RGB)
            .set_max_width(MAX_SOURCE_SIZE)
            .set_max_height(MAX_SOURCE_SIZE);
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(jpeg), options);
        let started = embassy_time::Instant::now();
        decoder.decode_headers()?;
        let size = decoder.output_buffer_size().expect("headers are decoded");
        // Only grows (rarely: pictures are all much the same size).
        rgb.resize(size, 0);
        decoder.decode_into(&mut rgb[..size])?;
        let info = decoder.info().expect("headers are decoded");
        let decoded = started.elapsed().as_millis();
        let picture = Self::from_rgb(&rgb[..size], info.width as usize, info.height as usize);
        log::info!(
            "{}x{} JPEG: decode {} ms, scale {} ms",
            info.width,
            info.height,
            decoded,
            started.elapsed().as_millis() - decoded
        );
        Ok(picture)
    }

    /// Scales an RGB888 image to the screen with bilinear filtering.
    fn from_rgb(rgb: &[u8], width: usize, height: usize) -> Self {
        let mut picture = Self::blank(slideshow::BACKGROUND);
        let placement = Placement::new(width, height);

        let mut i = 0;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (lx, ly) = panel_to_logical(x, y);
                if let Some((sx, sy)) = placement.source(lx, ly) {
                    let [r, g, b] = sample(rgb, width, height, sx, sy);
                    picture.pixels[i] = to_rgb565(r, g, b, x, y);
                }
                i += 1;
            }
        }
        picture
    }

    pub fn pixels(&self) -> &[u16] {
        &self.pixels
    }

    /// Writes `text` in the corner of the picture (see [`crate::caption`]).
    pub fn draw_caption(&mut self, text: &str) {
        crate::caption::draw(&mut self.pixels, text);
    }
}

/// Where a picture lands on the (logical) screen.
struct Placement {
    /// Top-left corner on screen; negative when `fill` crops the picture.
    left: i32,
    top: i32,
    /// Size on screen.
    width: i32,
    height: i32,
    /// Source pixels per screen pixel, 16.16 fixed point.
    step_x: i32,
    step_y: i32,
}

impl Placement {
    fn new(source_width: usize, source_height: usize) -> Self {
        let (screen_width, screen_height) = logical_size();
        // Compare aspect ratios without division: is the picture wider?
        let wider = source_width * screen_height > source_height * screen_width;
        let (width, height) = match (slideshow::SCALE, wider) {
            (Scale::Fit, true) | (Scale::Fill, false) => {
                (screen_width, (source_height * screen_width / source_width).max(1))
            }
            (Scale::Fit, false) | (Scale::Fill, true) => {
                ((source_width * screen_height / source_height).max(1), screen_height)
            }
        };
        Self {
            left: (screen_width as i32 - width as i32) / 2,
            top: (screen_height as i32 - height as i32) / 2,
            width: width as i32,
            height: height as i32,
            step_x: ((source_width << 16) / width) as i32,
            step_y: ((source_height << 16) / height) as i32,
        }
    }

    /// The source position (16.16 fixed point) shown at a screen pixel, or
    /// None if the pixel is outside the picture.
    #[inline(always)]
    fn source(&self, x: usize, y: usize) -> Option<(i32, i32)> {
        let px = x as i32 - self.left;
        let py = y as i32 - self.top;
        if px < 0 || py < 0 || px >= self.width || py >= self.height {
            return None;
        }
        // Sample at the centre of the screen pixel; pixel centres in the
        // source are at +0.5, so subtract that for the bilinear weights.
        let sx = px * self.step_x + self.step_x / 2 - 0x8000;
        let sy = py * self.step_y + self.step_y / 2 - 0x8000;
        Some((sx, sy))
    }
}

/// Bilinear sample of an RGB888 image at a 16.16 fixed-point position.
#[inline(always)]
fn sample(rgb: &[u8], width: usize, height: usize, sx: i32, sy: i32) -> [u8; 3] {
    let max_x = ((width - 1) << 16) as i32;
    let max_y = ((height - 1) << 16) as i32;
    let sx = sx.clamp(0, max_x);
    let sy = sy.clamp(0, max_y);
    let (x0, y0) = ((sx >> 16) as usize, (sy >> 16) as usize);
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    // 8-bit weights.
    let fx = ((sx >> 8) & 0xFF) as u32;
    let fy = ((sy >> 8) & 0xFF) as u32;

    let row0 = y0 * width;
    let row1 = y1 * width;
    let mut out = [0u8; 3];
    for (c, out) in out.iter_mut().enumerate() {
        let p00 = rgb[(row0 + x0) * 3 + c] as u32;
        let p10 = rgb[(row0 + x1) * 3 + c] as u32;
        let p01 = rgb[(row1 + x0) * 3 + c] as u32;
        let p11 = rgb[(row1 + x1) * 3 + c] as u32;
        let top = p00 * (256 - fx) + p10 * fx;
        let bottom = p01 * (256 - fx) + p11 * fx;
        *out = ((top * (256 - fy) + bottom * fy) >> 16) as u8;
    }
    out
}

/// 4x4 Bayer matrix, values 0..16.
const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Packs RGB888 into RGB565. With `[slideshow] dither` on, a 4x4 ordered
/// dither (keyed to the panel position, so the pattern holds still) spreads
/// the rounding error, which hides most of the banding in skies and
/// shadows.
#[inline(always)]
fn to_rgb565(r: u8, g: u8, b: u8, x: usize, y: usize) -> u16 {
    let (r, g, b) = if slideshow::DITHER {
        let t = BAYER[y & 3][x & 3];
        // Red and blue lose 3 bits (steps of 8), green 2 (steps of 4).
        (
            r.saturating_add(t >> 1),
            g.saturating_add(t >> 2),
            b.saturating_add(t >> 1),
        )
    } else {
        (r, g, b)
    };
    ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3)
}

/// Writes a mix of two screenfuls into `out`: `amount` 0 is all `from`,
/// [`FADE_STEPS`] is all `to`.
///
/// Each pixel is spread out as `00000gggggg00000rrrrr000000bbbbb`, which
/// leaves room for all three channels to be multiplied by up to 32 in one
/// 32-bit multiply.
pub fn blend(from: &[u16], to: &[u16], out: &mut [u16], amount: u32) {
    const MASK: u32 = 0x07E0_F81F;
    let amount = amount.min(FADE_STEPS);
    let keep = FADE_STEPS - amount;
    for ((out, &a), &b) in out.iter_mut().zip(from).zip(to) {
        let a = (a as u32 | (a as u32) << 16) & MASK;
        let b = (b as u32 | (b as u32) << 16) & MASK;
        let mixed = ((a * keep + b * amount) >> 5) & MASK;
        *out = (mixed | mixed >> 16) as u16;
    }
}
