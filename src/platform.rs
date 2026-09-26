//! Slint platform glue: time, rendering into the framebuffer, and touch.

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::slice;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, RenderingRotation, RepaintBufferType, Rgb565Pixel,
};
use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, PhysicalSize, PlatformError};

use crate::config::display::{ROTATION, WIDTH};
use crate::config::{Rotation, logical_size};
use crate::hw::framebuffer::FrameBuffer;
use crate::hw::gt911::Touch;
use crate::hw::touch_to_logical;

struct EspPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for EspPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        let micros = esp_hal::time::Instant::now()
            .duration_since_epoch()
            .as_micros();
        core::time::Duration::from_micros(micros)
    }

    fn debug_log(&self, arguments: core::fmt::Arguments) {
        log::info!("{arguments}");
    }
}

/// Installs the platform and returns the one window everything renders into.
pub fn init() -> Rc<MinimalSoftwareWindow> {
    // The framebuffers also carry pictures drawn outside Slint, so Slint
    // can't rely on what it drew last time: redraw the whole window each
    // frame. The menus are simple enough for that to be quick.
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let (width, height) = logical_size();
    window.set_size(PhysicalSize::new(width as u32, height as u32));
    slint::platform::set_platform(Box::new(EspPlatform {
        window: window.clone(),
    }))
    .expect("platform already set");
    window
}

/// Redraws the window into the back buffer and queues it for display, if
/// anything changed. Returns true if a frame was drawn; wait for
/// `fb.is_presented()` before drawing again.
pub fn render(window: &MinimalSoftwareWindow, fb: &mut FrameBuffer) -> bool {
    let drawn = window.draw_if_needed(|renderer| {
        renderer.set_rendering_rotation(match ROTATION {
            Rotation::Deg0 => RenderingRotation::NoRotation,
            Rotation::Deg90 => RenderingRotation::Rotate90,
            Rotation::Deg180 => RenderingRotation::Rotate180,
            Rotation::Deg270 => RenderingRotation::Rotate270,
        });
        let pixels = fb.pixels();
        // Rgb565Pixel is a #[repr(transparent)] u16.
        let pixels: &mut [Rgb565Pixel] =
            unsafe { slice::from_raw_parts_mut(pixels.as_mut_ptr().cast(), pixels.len()) };
        renderer.render(pixels, WIDTH);
    });
    if drawn {
        fb.present();
    }
    drawn
}

/// Turns GT911 polls into Slint pointer events, in logical coordinates.
#[derive(Default)]
pub struct TouchInput {
    last: Option<LogicalPosition>,
}

impl TouchInput {
    pub fn handle(&mut self, window: &MinimalSoftwareWindow, touch: Touch) {
        let window = window.window();
        match touch {
            Touch::NoChange => {}
            Touch::Pressed { x, y } => {
                let (x, y) = touch_to_logical(x, y);
                let position = LogicalPosition::new(x as f32, y as f32);
                let event = if self.last.is_some() {
                    WindowEvent::PointerMoved { position }
                } else {
                    WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    }
                };
                self.last = Some(position);
                window.dispatch_event(event);
            }
            Touch::Released => {
                if let Some(position) = self.last.take() {
                    window.dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                    window.dispatch_event(WindowEvent::PointerExited);
                }
            }
        }
    }
}
