//! Text on a dark panel in a bottom corner of the screen: the picture's
//! three-word phrase on the left (`[slideshow] show_phrase`, or "Show
//! picture names" in the menu) and the clock on the right (see
//! [`crate::clock`]).
//!
//! Pictures are drawn without Slint, so the glyphs come from the UI font,
//! pre-rendered by `build.rs`. Integer arithmetic only (see Cargo.toml).

use crate::config::{logical_size, logical_to_panel_index};

struct Glyph {
    /// Start of the glyph's coverage bitmap in [`font::BITMAP`].
    offset: u32,
    width: u8,
    height: u8,
    /// From the pen position to the bitmap's left edge.
    left: i8,
    /// From the baseline up to the bitmap's top edge.
    top: i8,
    advance: u8,
}

mod font {
    use super::Glyph;
    include!(concat!(env!("OUT_DIR"), "/caption_font.rs"));
}

/// Space around the panel, and inside it around the text.
pub const MARGIN: i32 = 16;
const PAD_X: i32 = 14;
const PAD_Y: i32 = 6;
const RADIUS: i32 = 8;
/// How much of the picture shows through the panel, out of 256.
const PANEL_SHOW_THROUGH: u32 = 90;

/// Which bottom corner the panel goes in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    Left,
    Right,
}

fn glyph(c: char) -> &'static Glyph {
    let i = if (' '..='~').contains(&c) { c as usize - ' ' as usize } else { '?' as usize - ' ' as usize };
    &font::GLYPHS[i]
}

/// How far the pen moves for `c`. Digits all take the width of the widest,
/// so the clock doesn't shift sideways as it ticks.
fn advance(c: char) -> i32 {
    if c.is_ascii_digit() {
        ('0'..='9').map(|d| glyph(d).advance as i32).max().unwrap_or(0)
    } else {
        glyph(c).advance as i32
    }
}

/// The width of the panel for `text`, if it all fits.
pub fn panel_width(text: &str) -> i32 {
    text.chars().map(advance).sum::<i32>() + 2 * PAD_X
}

/// Draws `text` into a screenful of panel-order RGB565 pixels, in `corner`.
/// The panel is at most `max_width` wide; text that doesn't fit is cut off.
pub fn draw(pixels: &mut [u16], text: &str, corner: Corner, max_width: i32) {
    let (screen_width, screen_height) = logical_size();
    let (screen_width, screen_height) = (screen_width as i32, screen_height as i32);

    // Lay out: as much of the text as fits.
    let max_text = max_width.min(screen_width - 2 * MARGIN) - 2 * PAD_X;
    let mut text_width = 0;
    let mut chars = 0;
    for c in text.chars() {
        let advance = advance(c);
        if text_width + advance > max_text {
            break;
        }
        text_width += advance;
        chars += 1;
    }

    let panel_width = text_width + 2 * PAD_X;
    let panel_height = font::LINE_HEIGHT + 2 * PAD_Y;
    let left = match corner {
        Corner::Left => MARGIN,
        Corner::Right => screen_width - MARGIN - panel_width,
    };
    let top = screen_height - MARGIN - panel_height;

    let mut set = |x: i32, y: i32, f: &dyn Fn(u16) -> u16| {
        if x >= 0 && y >= 0 && x < screen_width && y < screen_height {
            let i = logical_to_panel_index(x as usize, y as usize);
            pixels[i] = f(pixels[i]);
        }
    };

    // The panel: darken the picture behind it, with rounded corners.
    for y in 0..panel_height {
        for x in 0..panel_width {
            if outside_corner(x, y, panel_width, panel_height) {
                continue;
            }
            set(left + x, top + y, &|p| scale(p, PANEL_SHOW_THROUGH));
        }
    }

    // The text, in white.
    let baseline = top + PAD_Y + font::ASCENT;
    let mut pen = left + PAD_X;
    for c in text.chars().take(chars) {
        let g = glyph(c);
        // Centred in its space, for digits wider than their glyph.
        let x0 = pen + (advance(c) - g.advance as i32) / 2 + g.left as i32;
        let y0 = baseline - g.top as i32;
        for gy in 0..g.height as usize {
            for gx in 0..g.width as usize {
                let coverage = font::BITMAP[g.offset as usize + gy * g.width as usize + gx];
                if coverage != 0 {
                    set(x0 + gx as i32, y0 + gy as i32, &|p| toward_white(p, coverage as u32));
                }
            }
        }
        pen += advance(c);
    }
}

/// True for points of a `width` x `height` box that its rounded corners cut
/// off.
fn outside_corner(x: i32, y: i32, width: i32, height: i32) -> bool {
    let cx = if x < RADIUS { RADIUS - 1 - x } else if x >= width - RADIUS { x - (width - RADIUS) } else { return false };
    let cy = if y < RADIUS { RADIUS - 1 - y } else if y >= height - RADIUS { y - (height - RADIUS) } else { return false };
    cx * cx + cy * cy > RADIUS * RADIUS
}

/// Scales an RGB565 pixel's brightness by `amount`/256.
fn scale(p: u16, amount: u32) -> u16 {
    let (r, g, b) = split(p);
    join(r * amount >> 8, g * amount >> 8, b * amount >> 8)
}

/// Mixes an RGB565 pixel toward white by `coverage`/255.
fn toward_white(p: u16, coverage: u32) -> u16 {
    let (r, g, b) = split(p);
    join(
        r + ((31 - r) * coverage) / 255,
        g + ((63 - g) * coverage) / 255,
        b + ((31 - b) * coverage) / 255,
    )
}

fn split(p: u16) -> (u32, u32, u32) {
    let p = p as u32;
    (p >> 11, (p >> 5) & 0x3F, p & 0x1F)
}

fn join(r: u32, g: u32, b: u32) -> u16 {
    ((r << 11) | (g << 5) | b) as u16
}
