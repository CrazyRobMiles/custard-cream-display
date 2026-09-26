# custard-cream-display

A touch-screen picture frame for pictures on a
[custard cream server](https://github.com/CrazyRobMiles/custard-cream-server).
It picks random pictures from the server, fades from one to the next, and
lets you choose which tags to show by tapping the screen.

It runs on the Waveshare **ESP32-S3-Touch-LCD-7** (800×480 RGB panel, GT911
touch). It is `no_std` Rust on [esp-hal](https://github.com/esp-rs/esp-hal),
with a [Slint](https://slint.dev) UI for the menus. The display and touch
drivers come from the `panel-demo` port in the ESP32-Panel project.

## Setting up

### 1. Wi-Fi credentials: `secrets.toml`

Wi-Fi details live in `secrets.toml`, which git ignores, so they never
reach the repository. Start from the committed example:

```powershell
copy secrets.example.toml secrets.toml
```

Then edit it. Add one `[[wifi]]` section for each network:

```toml
[[wifi]]
ssid = "my-home-network"
password = "my-wifi-password"

[[wifi]]
ssid = "my-phone-hotspot"
password = "another-password"
```

| Key        | Meaning                                                              |
|------------|----------------------------------------------------------------------|
| `ssid`     | Network name, 1 to 32 bytes, case-sensitive.                         |
| `password` | WPA/WPA2 passphrase (8 to 63 characters), or `""` for an open network. |

- **Order:** the frame tries the networks in order and joins the first one
  that answers. If the connection drops, it starts again from the top.
- **Supported security:** WPA2 and WPA2/WPA3 "transition" networks work. The
  Wi-Fi driver can't join WPA3-only networks yet.
- **Build-time only:** `build.rs` reads the file when the firmware is compiled
  and builds the values into the program. After editing it, rebuild and
  reflash. A missing file, a missing network or a bad password length stops
  the build with a message saying what to fix.
- **Not encrypted:** the credentials are stored in the firmware in plain text.
  Anyone with the binary, or with the board and a USB cable, can read them.

This follows the usual Rust embedded convention: a TOML file kept out of
git, read at build time, with a committed `.example` next to it.

### 2. Settings: `settings.toml`

Everything that isn't secret is in `settings.toml`, which is committed. Like
the secrets, it is read and checked at build time. Mistakes such as a
misspelt key, an unsupported display type or an odd rotation are build errors.

| Section        | Key                    | Default                             | Meaning |
|----------------|------------------------|-------------------------------------|---------|
| `[server]`     | `url`                  | `"http://custardcream.cam/"`        | Server base address. `http://` only (see [Limitations](#limitations)). |
| `[display]`    | `type`                 | `"waveshare-esp32-s3-touch-lcd-7"`  | The display hardware. This is the only type so far. |
|                | `width`, `height`      | from `type` (800, 480)              | Panel size in pixels, as the panel scans out. |
|                | `rotation`             | `0`                                 | Degrees clockwise: `0`, `90`, `180`, `270`. `90`/`270` make a portrait frame; pictures, menus and touch all turn to match. |
| `[display.timing]` | `pixel_clock_hz`, `hsync_pulse`, `hsync_back_porch`, `hsync_front_porch`, `vsync_pulse`, `vsync_back_porch`, `vsync_front_porch` | from `type` | Panel timing overrides. Leave the section out to use the defaults. |
| `[touch]`      | `swap_xy`, `invert_x`, `invert_y` | `false`                  | Corrections if taps land in the wrong place. They apply before `rotation`. |
| `[slideshow]`  | `seconds_per_picture`  | `20`                                | How long each picture shows (at least 3). |
|                | `transition`           | `"crossfade"`                       | `"crossfade"` or `"cut"`. |
|                | `fade_milliseconds`    | `1500`                              | Crossfade length (at most half of `seconds_per_picture`). |
|                | `scale`                | `"fit"`                             | `"fit"` shows the whole picture with borders; `"fill"` crops to fill the screen. |
|                | `background`           | `"#000000"`                         | Border colour for `"fit"`. |
|                | `dither`               | `true`                              | Ordered dithering to hide 16-bit colour banding. |
|                | `tags`                 | `[]`                                | Tags to start with. Empty means every picture. |
|                | `menu_timeout_seconds` | `60`                                | The tag menu closes after this long without a touch. |
|                | `show_phrase`          | `false`                             | Write each picture's three-word phrase in the corner. "Show picture names" in the menu changes it on the frame. |

`build.rs` turns both files into constants in `$OUT_DIR/config.rs`, which
[src/config.rs](src/config.rs) includes. The panel size is a constant, so
it can size the static DMA buffers. `build.rs` also works out the DMA
descriptor and bounce-buffer sizes for the panel, and rejects panel sizes
the scan-out can't handle or that wouldn't fit in PSRAM.

### 3. Build and flash

You need the Xtensa Rust toolchain (`espup install --targets esp32s3`) and
`espflash`. In PowerShell:

```powershell
. $env:USERPROFILE\export-esp.ps1
cargo build --release
espflash flash --chip esp32s3 --port COM4 target\xtensa-esp32s3-none-elf\release\custard-cream-display
```

`cargo run --release` builds, flashes and opens the serial monitor in one
step. The log shows Wi-Fi progress, each picture as it's decoded and shown,
and how many steps each fade managed.

## Using the frame

- **Starting up:** until the first picture arrives, the screen shows what the
  frame is doing: joining Wi-Fi, its IP address, and any trouble reaching the
  server.
- **Pictures:** the frame shows random pictures from the server, one every
  `seconds_per_picture`, fading from each to the next.
- **Choosing tags:** tap the screen to open the menu. Tap tags to turn them on
  or off, then tap **Done**.
  - A picture shows if it has *any* of the chosen tags.
  - **All pictures** clears the choice.
  - New tags appear in the menu as the server gets them.
  - The menu closes by itself after `menu_timeout_seconds` without a touch,
    keeping the old choice.

## How it works

| Task | Does |
|------|------|
| `network::wifi_task` | Joins Wi-Fi from `secrets.toml` and keeps it up; the embassy-net stack gets an address over DHCP. |
| `fetcher::fetcher_task` | Keeps the next picture ready. It asks the server for a batch of random pictures with the chosen tags and downloads one. It starts each download only after the previous picture has faded in, so downloads never slow a fade. |
| `fetcher::start_decoder` (thread) | Decodes the JPEG and fits it to the screen, which takes about 2 s for an 800×600 picture. It runs while the previous picture is showing, in a lowest-priority thread, so the display loop and touch carry on as normal. |
| `frame::run` (main) | Runs the screen: pictures, transitions, touch and the Slint menus. |

The frame uses three endpoints on the server, none of which needs a login:

| Request                               | Used for                            |
|---------------------------------------|-------------------------------------|
| `GET /random/tags`                    | The tags in the menu                |
| `GET /random/batch?count=10&tags=a,b` | Choosing random pictures            |
| `GET /pictures/<phrase>/thumb?size=800` | The picture, as a resized JPEG (the server fits it inside `size`×`size`) |

### Pictures and fading

The panel is RGB565, 16 bits per pixel, which limits how smooth the pictures
and fades can be:

- **Preparing a picture:** the JPEG is decoded with
  [zune-jpeg](https://crates.io/crates/zune-jpeg), then scaled with bilinear
  filtering and rotated for `rotation`. It is dithered down to RGB565 with a
  4×4 ordered dither, then kept in PSRAM in the panel's own pixel order.
- **Crossfading:** every step of a fade redraws the whole back buffer as a mix
  of the old and new pictures, then swaps it in at the next VSYNC.
  - The mix works on all three colour channels of a pixel in one 32-bit
    multiply.
  - With 5 bits of red and blue, a fade can have at most 32 distinct steps.
  - Each step reads two pictures and writes one screenful through PSRAM,
    while the panel is also reading PSRAM at 32 MB/s. A step takes about
    125 ms on the 800×480 panel, so a 1.5 s fade shows about 13 steps.
  - Steps are timed against the clock, so a slow step skips ahead rather than
    stretching the fade. The log reports how many steps each fade drew.
- **Scan-out:** the display uses the bounce-buffer scan-out and double
  buffering from `panel-demo`, described in its README. This is why pictures
  and fades never tear or lose sync.

### Memory

- **Framebuffers:** two, 750 KB each, at the start of PSRAM.
- **Heap:** the rest of PSRAM, holding:
  - the picture on screen and the one waiting (750 KB each);
  - the JPEG download;
  - the RGB888 image while it's being decoded (up to 1.9 MB for 800×800).

The UI only draws while the status page or the menu is up. It redraws the
whole screen each time, because pictures are drawn into the same
framebuffers outside Slint.

## Limitations

- **No HTTPS:** there is no TLS yet, so the server must answer on `http://`.
  The public custard cream server does, through Cloudflare. HTTPS would need
  `embedded-tls` or `esp-mbedtls`, plus certificate handling.
- **Menu choices are forgotten at power-off:** the frame goes back to `tags`
  and `show_phrase` from `settings.toml`. Saving it would need a small flash key-value store,
  such as `sequential-storage` on `esp-storage`.
- **Fades are stepped:** at about 8 steps a second, a fade is visibly
  stepped rather than perfectly smooth (see [Pictures and fading](#pictures-and-fading)).
- **`fill` and portrait pictures:** in `"fill"` mode the server's thumbnail
  is fitted inside a square the size of the screen's longer side. Portrait
  pictures on a landscape screen are therefore scaled up a little.
- **One display type:** other ESP32-S3 RGB boards need their own
  `[display] type`, which means a preset in `build.rs`, a `DisplayType`
  variant and bring-up code in `src/bin/main.rs`.

## Layout

| File                    | Purpose                                                    |
|-------------------------|------------------------------------------------------------|
| `secrets.example.toml`  | Template for `secrets.toml` (Wi-Fi credentials, not committed) |
| `settings.toml`         | Server, display hardware and slideshow settings            |
| `build.rs`              | Checks both files and generates `config.rs`; compiles the UI |
| `src/bin/main.rs`       | Start-up: PSRAM, heap, Wi-Fi, display, tasks               |
| `src/config.rs`         | Types for the generated settings; rotation helpers         |
| `src/frame.rs`          | Display loop: pictures, transitions, menus, touch          |
| `src/picture.rs`        | JPEG decode, scaling, dithering, crossfade mixing          |
| `src/fetcher.rs`        | Fetcher task, and the decoder task                         |
| `src/server.rs`         | The custard cream server's API                             |
| `src/http.rs`           | Minimal HTTP/1.1 client                                    |
| `src/network.rs`        | Wi-Fi connection and the network stack task                |
| `src/state.rs`          | State shared between the tasks                             |
| `src/platform.rs`       | Slint platform: rendering, rotation, touch input           |
| `src/hw/`               | Board wiring, framebuffer scan-out, CH422G, GT911          |
| `ui/app.slint`          | Status page and tag menu                                   |
| `patches/i-slint-core`  | Slint 1.18.1 with a `no_std` fix (`f32::rem_euclid`)       |
| `patches/esp-radio`     | esp-radio 1.0.0-beta.1 with a fix for a Wi-Fi TX stall     |

## Toolchain notes

These carry over from `panel-demo`:

- **Soft-float for this crate:** the Xtensa LLVM backend crashes on some
  Slint-generated float code, so this crate is compiled with
  `-C target-feature=-fp`, with LTO off. The pixel code here is all integer
  arithmetic for that reason. Dependencies, including the JPEG decoder,
  still use the FPU.
- **Windows on ARM:** the Xtensa `rustc` runs under x64 emulation there and
  needs a bigger stack, which `.cargo/config.toml` sets with
  `RUST_MIN_STACK`.

## Licences

Montserrat (`ui/fonts`) is under the SIL Open Font License
(`ui/fonts/OFL.txt`). Slint is available under GPLv3 or a commercial
licence; its royalty-free licence does not cover embedded devices.
