//! Build script: compiles the Slint UI, turns `settings.toml` and
//! `secrets.toml` into Rust constants (`$OUT_DIR/config.rs`, included by
//! `src/config.rs`), and pre-renders the caption font
//! (`$OUT_DIR/caption_font.rs`, included by `src/caption.rs`).
//!
//! Both files are checked here, so a mistake is a build error with a
//! readable message rather than a misbehaving frame.

use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;

const SETTINGS_FILE: &str = "settings.toml";
const SECRETS_FILE: &str = "secrets.toml";
const SECRETS_EXAMPLE: &str = "secrets.example.toml";

fn main() {
    linker_be_nice();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");

    println!("cargo:rerun-if-changed={SETTINGS_FILE}");
    println!("cargo:rerun-if-changed={SECRETS_FILE}");

    let settings: Settings = read_toml(SETTINGS_FILE, None);
    let secrets: Secrets = read_toml(SECRETS_FILE, Some(SECRETS_EXAMPLE));
    let config = generate(&settings, &secrets).unwrap_or_else(|e| fail(&e));
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(Path::new(&out_dir).join("config.rs"), config).unwrap();

    println!("cargo:rerun-if-changed={CAPTION_FONT}");
    std::fs::write(Path::new(&out_dir).join("caption_font.rs"), caption_font()).unwrap();

    // Pre-render fonts for the MCU software renderer.
    let slint_config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer)
        .with_style("material".into());
    slint_build::compile_with_config("ui/app.slint", slint_config).unwrap();
}

// ---------------------------------------------------------------------------
// File formats. `deny_unknown_fields` turns a misspelt setting into an error
// instead of it being silently ignored.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Secrets {
    wifi: Vec<WifiNetwork>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WifiNetwork {
    ssid: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    server: Server,
    display: Display,
    #[serde(default)]
    touch: Touch,
    #[serde(default)]
    slideshow: Slideshow,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Server {
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Display {
    #[serde(rename = "type")]
    kind: String,
    width: Option<usize>,
    height: Option<usize>,
    #[serde(default)]
    rotation: u32,
    #[serde(default)]
    timing: TimingOverrides,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TimingOverrides {
    pixel_clock_hz: Option<u32>,
    hsync_pulse: Option<usize>,
    hsync_back_porch: Option<usize>,
    hsync_front_porch: Option<usize>,
    vsync_pulse: Option<usize>,
    vsync_back_porch: Option<usize>,
    vsync_front_porch: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Touch {
    #[serde(default)]
    swap_xy: bool,
    #[serde(default)]
    invert_x: bool,
    #[serde(default)]
    invert_y: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Slideshow {
    seconds_per_picture: u32,
    transition: String,
    fade_milliseconds: u32,
    scale: String,
    background: String,
    dither: bool,
    tags: Vec<String>,
    menu_timeout_seconds: u32,
    show_phrase: bool,
}

impl Default for Slideshow {
    fn default() -> Self {
        Self {
            seconds_per_picture: 20,
            transition: "crossfade".into(),
            fade_milliseconds: 1500,
            scale: "fit".into(),
            background: "#000000".into(),
            dither: true,
            tags: Vec::new(),
            menu_timeout_seconds: 60,
            show_phrase: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Supported display hardware. To add a board, add an entry here and a
// matching `DisplayType` variant and bring-up code in `src/hw/`.

struct DisplayPreset {
    name: &'static str,
    variant: &'static str,
    width: usize,
    height: usize,
    timing: Timing,
}

struct Timing {
    pixel_clock_hz: u32,
    hsync_pulse: usize,
    hsync_back_porch: usize,
    hsync_front_porch: usize,
    vsync_pulse: usize,
    vsync_back_porch: usize,
    vsync_front_porch: usize,
}

const DISPLAYS: &[DisplayPreset] = &[DisplayPreset {
    name: "waveshare-esp32-s3-touch-lcd-7",
    variant: "WaveshareEsp32S3TouchLcd7",
    width: 800,
    height: 480,
    timing: Timing {
        pixel_clock_hz: 16_000_000,
        hsync_pulse: 4,
        hsync_back_porch: 8,
        hsync_front_porch: 8,
        vsync_pulse: 4,
        vsync_back_porch: 8,
        vsync_front_porch: 8,
    },
}];

/// PSRAM on the ESP32-S3 modules this runs on, less room for the heap
/// (JPEG download and decode).
const PSRAM_FOR_PIXELS: usize = 8 * 1024 * 1024 - 2 * 1024 * 1024;

/// A DMA descriptor can hold at most 4095 bytes.
const MAX_DESCRIPTOR_BYTES: usize = 4095;

/// Largest bounce buffer, in lines; there are two, in internal SRAM.
const MAX_BOUNCE_LINES: usize = 10;

// ---------------------------------------------------------------------------

fn generate(settings: &Settings, secrets: &Secrets) -> Result<String, String> {
    let mut out = String::new();
    writeln!(out, "// Generated by build.rs from {SETTINGS_FILE} and {SECRETS_FILE}. Do not edit.").unwrap();

    // Secrets
    if secrets.wifi.is_empty() {
        return Err(format!("{SECRETS_FILE}: add at least one [[wifi]] network"));
    }
    writeln!(out, "pub mod secrets {{").unwrap();
    writeln!(out, "    pub const WIFI_NETWORKS: &[super::WifiNetwork] = &[").unwrap();
    for (i, net) in secrets.wifi.iter().enumerate() {
        let n = i + 1;
        if net.ssid.is_empty() || net.ssid.len() > 32 {
            return Err(format!("{SECRETS_FILE}: Wi-Fi network {n}: the ssid must be 1 to 32 bytes long"));
        }
        let len = net.password.len();
        let hex_key = len == 64 && net.password.bytes().all(|b| b.is_ascii_hexdigit());
        if len != 0 && !(8..=63).contains(&len) && !hex_key {
            return Err(format!(
                "{SECRETS_FILE}: Wi-Fi network {n} ({:?}): the password must be 8 to 63 characters, \
                 or \"\" for an open network",
                net.ssid
            ));
        }
        writeln!(
            out,
            "        super::WifiNetwork {{ ssid: {:?}, password: {:?} }},",
            net.ssid, net.password
        )
        .unwrap();
    }
    writeln!(out, "    ];\n}}").unwrap();

    // Server
    let (host, port, path) = parse_url(&settings.server.url)?;
    writeln!(out, "pub mod server {{").unwrap();
    writeln!(out, "    pub const URL: &str = {:?};", settings.server.url).unwrap();
    writeln!(out, "    pub const HOST: &str = {host:?};").unwrap();
    writeln!(out, "    pub const PORT: u16 = {port};").unwrap();
    writeln!(out, "    /// Always starts and ends with '/'.").unwrap();
    writeln!(out, "    pub const BASE_PATH: &str = {path:?};").unwrap();
    writeln!(out, "}}").unwrap();

    // Display
    let display = &settings.display;
    let Some(preset) = DISPLAYS.iter().find(|p| p.name == display.kind) else {
        let names: Vec<_> = DISPLAYS.iter().map(|p| format!("{:?}", p.name)).collect();
        return Err(format!(
            "{SETTINGS_FILE}: [display] type {:?} is not supported. Supported types: {}",
            display.kind,
            names.join(", ")
        ));
    };
    let width = display.width.unwrap_or(preset.width);
    let height = display.height.unwrap_or(preset.height);
    if width == 0 || height == 0 || height % 2 != 0 {
        return Err(format!(
            "{SETTINGS_FILE}: [display] {width}x{height}: width and height must be non-zero, \
             and the height a multiple of 2"
        ));
    }
    // Two framebuffers and two decoded pictures, 2 bytes per pixel.
    if 4 * width * height * 2 > PSRAM_FOR_PIXELS {
        return Err(format!("{SETTINGS_FILE}: [display] {width}x{height} is too big to fit in PSRAM"));
    }
    let rotation = match display.rotation {
        0 => "Deg0",
        90 => "Deg90",
        180 => "Deg180",
        270 => "Deg270",
        r => return Err(format!("{SETTINGS_FILE}: [display] rotation must be 0, 90, 180 or 270, not {r}")),
    };
    let (descriptor_lines, bounce_lines) = bounce_geometry(width, height).ok_or_else(|| {
        format!(
            "{SETTINGS_FILE}: [display] {width}x{height}: no bounce buffer size fits this panel. \
             The height needs a divisor of 2 to {MAX_BOUNCE_LINES} lines whose size is a \
             multiple of 32 bytes"
        )
    })?;
    let t = &display.timing;
    let p = &preset.timing;
    writeln!(out, "pub mod display {{").unwrap();
    writeln!(out, "    pub const TYPE: super::DisplayType = super::DisplayType::{};", preset.variant).unwrap();
    writeln!(out, "    pub const WIDTH: usize = {width};").unwrap();
    writeln!(out, "    pub const HEIGHT: usize = {height};").unwrap();
    writeln!(out, "    pub const ROTATION: super::Rotation = super::Rotation::{rotation};").unwrap();
    let timings = [
        ("PIXEL_CLOCK_HZ", "u32", t.pixel_clock_hz.unwrap_or(p.pixel_clock_hz) as usize),
        ("HSYNC_PULSE", "usize", t.hsync_pulse.unwrap_or(p.hsync_pulse)),
        ("HSYNC_BACK_PORCH", "usize", t.hsync_back_porch.unwrap_or(p.hsync_back_porch)),
        ("HSYNC_FRONT_PORCH", "usize", t.hsync_front_porch.unwrap_or(p.hsync_front_porch)),
        ("VSYNC_PULSE", "usize", t.vsync_pulse.unwrap_or(p.vsync_pulse)),
        ("VSYNC_BACK_PORCH", "usize", t.vsync_back_porch.unwrap_or(p.vsync_back_porch)),
        ("VSYNC_FRONT_PORCH", "usize", t.vsync_front_porch.unwrap_or(p.vsync_front_porch)),
    ];
    for (name, ty, value) in timings {
        writeln!(out, "    pub const {name}: {ty} = {value};").unwrap();
    }
    writeln!(out, "    /// Derived: lines per DMA descriptor.").unwrap();
    writeln!(out, "    pub const DESCRIPTOR_LINES: usize = {descriptor_lines};").unwrap();
    writeln!(out, "    /// Derived: lines per bounce buffer.").unwrap();
    writeln!(out, "    pub const BOUNCE_LINES: usize = {bounce_lines};").unwrap();
    writeln!(out, "}}").unwrap();

    // Touch
    let touch = &settings.touch;
    writeln!(out, "pub mod touch {{").unwrap();
    writeln!(out, "    pub const SWAP_XY: bool = {};", touch.swap_xy).unwrap();
    writeln!(out, "    pub const INVERT_X: bool = {};", touch.invert_x).unwrap();
    writeln!(out, "    pub const INVERT_Y: bool = {};", touch.invert_y).unwrap();
    writeln!(out, "}}").unwrap();

    // Slideshow
    let show = &settings.slideshow;
    if show.seconds_per_picture < 3 {
        return Err(format!("{SETTINGS_FILE}: [slideshow] seconds_per_picture must be at least 3"));
    }
    let transition = match show.transition.as_str() {
        "crossfade" => "Crossfade",
        "cut" => "Cut",
        other => {
            return Err(format!(
                "{SETTINGS_FILE}: [slideshow] transition must be \"crossfade\" or \"cut\", not {other:?}"
            ));
        }
    };
    if show.fade_milliseconds > show.seconds_per_picture * 1000 / 2 {
        return Err(format!(
            "{SETTINGS_FILE}: [slideshow] fade_milliseconds must be no more than half of seconds_per_picture"
        ));
    }
    let scale = match show.scale.as_str() {
        "fit" => "Fit",
        "fill" => "Fill",
        other => return Err(format!("{SETTINGS_FILE}: [slideshow] scale must be \"fit\" or \"fill\", not {other:?}")),
    };
    let background = parse_colour(&show.background).ok_or_else(|| {
        format!(
            "{SETTINGS_FILE}: [slideshow] background must be a colour like \"#000000\", not {:?}",
            show.background
        )
    })?;
    for tag in &show.tags {
        if tag.trim().is_empty() || tag.contains(',') || tag.trim() != tag {
            return Err(format!(
                "{SETTINGS_FILE}: [slideshow] tag {tag:?}: tags can't be empty, contain commas, \
                 or start or end with spaces"
            ));
        }
    }
    if show.menu_timeout_seconds < 5 {
        return Err(format!("{SETTINGS_FILE}: [slideshow] menu_timeout_seconds must be at least 5"));
    }
    writeln!(out, "pub mod slideshow {{").unwrap();
    writeln!(out, "    pub const SECONDS_PER_PICTURE: u64 = {};", show.seconds_per_picture).unwrap();
    writeln!(out, "    pub const TRANSITION: super::Transition = super::Transition::{transition};").unwrap();
    writeln!(out, "    pub const FADE_MILLISECONDS: u64 = {};", show.fade_milliseconds).unwrap();
    writeln!(out, "    pub const SCALE: super::Scale = super::Scale::{scale};").unwrap();
    writeln!(out, "    /// RGB565.").unwrap();
    writeln!(out, "    pub const BACKGROUND: u16 = {background:#06x};").unwrap();
    writeln!(out, "    pub const DITHER: bool = {};", show.dither).unwrap();
    writeln!(out, "    pub const TAGS: &[&str] = &{:?};", show.tags).unwrap();
    writeln!(out, "    pub const MENU_TIMEOUT_SECONDS: u64 = {};", show.menu_timeout_seconds).unwrap();
    writeln!(out, "    pub const SHOW_PHRASE: bool = {};", show.show_phrase).unwrap();
    writeln!(out, "}}").unwrap();

    Ok(out)
}

// ---------------------------------------------------------------------------
// Caption font. Pictures are drawn without Slint, so the phrase caption uses
// glyphs rendered here from the UI font: printable ASCII, as 8-bit coverage
// bitmaps.

const CAPTION_FONT: &str = "ui/fonts/Montserrat-SemiBold.ttf";
const CAPTION_PX: f32 = 26.0;

fn caption_font() -> String {
    let data = std::fs::read(CAPTION_FONT).unwrap_or_else(|e| fail(&format!("{CAPTION_FONT}: {e}")));
    let font = fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
        .unwrap_or_else(|e| fail(&format!("{CAPTION_FONT}: {e}")));
    let line = font.horizontal_line_metrics(CAPTION_PX).unwrap();

    let mut glyphs = String::new();
    let mut bitmap: Vec<u8> = Vec::new();
    for c in ' '..='~' {
        let (m, coverage) = font.rasterize(c, CAPTION_PX);
        writeln!(
            glyphs,
            "    Glyph {{ offset: {}, width: {}, height: {}, left: {}, top: {}, advance: {} }},",
            bitmap.len(),
            m.width,
            m.height,
            m.xmin,
            // fontdue gives the bitmap's bottom relative to the baseline.
            m.ymin + m.height as i32,
            m.advance_width.round() as i32,
        )
        .unwrap();
        bitmap.extend_from_slice(&coverage);
    }

    let mut out = String::new();
    writeln!(out, "// Generated by build.rs from {CAPTION_FONT} at {CAPTION_PX}px.").unwrap();
    writeln!(out, "pub const ASCENT: i32 = {};", line.ascent.round() as i32).unwrap();
    writeln!(out, "pub const LINE_HEIGHT: i32 = {};", (line.ascent - line.descent).round() as i32).unwrap();
    writeln!(out, "/// Glyphs for ' '..='~'.").unwrap();
    writeln!(out, "pub static GLYPHS: [Glyph; 95] = [
{glyphs}];").unwrap();
    writeln!(out, "pub static BITMAP: [u8; {}] = {:?};", bitmap.len(), bitmap).unwrap();
    out
}

/// Splits `http://host[:port][/path]` into host, port and a path that
/// starts and ends with '/'.
fn parse_url(url: &str) -> Result<(String, u16, String), String> {
    let bad = |why: &str| format!("{SETTINGS_FILE}: [server] url {url:?}: {why}");
    if url.starts_with("https://") {
        return Err(bad("https isn't supported yet; use the http:// address instead"));
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| bad("it must start with http://"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| bad("the port isn't a number"))?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err(bad("there is no host name"));
    }
    let path = path.trim_end_matches('/');
    let path = if path.is_empty() { "/".to_string() } else { format!("/{path}/") };
    Ok((host.to_string(), port, path))
}

/// "#rrggbb" to RGB565.
fn parse_colour(colour: &str) -> Option<u16> {
    let hex = colour.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
    Some(((r >> 3) << 11 | (g >> 2) << 5 | (b >> 3)) as u16)
}

/// Lines per DMA descriptor and per bounce buffer for this panel, or None if
/// nothing fits. See `src/hw/framebuffer.rs`.
fn bounce_geometry(width: usize, height: usize) -> Option<(usize, usize)> {
    let line_bytes = width * 2;
    let descriptor_lines = (1..=2).rev().find(|n| n * line_bytes <= MAX_DESCRIPTOR_BYTES)?;
    // The refill loop copies 8 words (32 bytes) at a time.
    let bounce_lines = (1..=MAX_BOUNCE_LINES)
        .rev()
        .filter(|l| l % descriptor_lines == 0)
        .find(|l| height % l == 0 && (l * line_bytes) % 32 == 0)?;
    Some((descriptor_lines, bounce_lines))
}

fn read_toml<T: serde::de::DeserializeOwned>(file: &str, example: Option<&str>) -> T {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(_) => match example {
            Some(example) => fail(&format!(
                "{file} is missing. Copy {example} to {file} and put your Wi-Fi details in it."
            )),
            None => fail(&format!("{file} is missing.")),
        },
    };
    toml::from_str(&text).unwrap_or_else(|e| fail(&format!("{file}: {e}")))
}

fn fail(message: &str) -> ! {
    eprintln!("\nerror: {message}\n");
    std::process::exit(1);
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `esp-radio` has no scheduler enabled. Make sure you have initialized `esp-rtos` or provided an external scheduler."
                    );
                    eprintln!();
                }
                "free" | "malloc" | "calloc" | "get_free_internal_heap_size" | "malloc_internal"
                | "realloc_internal" | "calloc_internal" | "free_internal" => {
                    eprintln!();
                    eprintln!(
                        "💡 Did you forget the `esp-alloc` dependency or didn't enable the `compat` feature on it?"
                    );
                    eprintln!();
                }
                _ => (),
            },
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
