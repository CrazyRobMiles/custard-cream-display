//! The display loop. The screen shows one of:
//!
//! - **Status** (Slint): until the first picture arrives.
//! - **Pictures** (drawn directly): each picture stays up for
//!   `seconds_per_picture`, then the next one fades in. The clock, if it is
//!   on, is drawn over them. A tap opens the menu.
//! - **Tag menu** (Slint): choose which tags to show, whether to write each
//!   picture's phrase on it, and whether to show the clock. It closes on
//!   "Done", or after
//!   `menu_timeout_seconds` without a touch (keeping the old choices).

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::clock;
use crate::config::{Transition, slideshow};
use crate::hw::framebuffer::FrameBuffer;
use crate::hw::{I2cBus, gt911};
use crate::picture::{self, FADE_STEPS, Picture};
use crate::platform::{self, TouchInput};
use crate::state::{self, NEXT_PICTURE, PICTURE_TAKEN};
use crate::store::{Choices, Store};
use crate::ui::{AppWindow, TagChoice};

const SHOW_TIME: Duration = Duration::from_secs(slideshow::SECONDS_PER_PICTURE);
const MENU_TIMEOUT: Duration = Duration::from_secs(slideshow::MENU_TIMEOUT_SECONDS);

/// How often to check the touch screen while a picture is showing.
const PICTURE_POLL: Duration = Duration::from_millis(20);

/// Something the UI asked for, from a Slint callback.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Request {
    #[default]
    None,
    OpenMenu,
    Done,
}

pub async fn run(
    app: &AppWindow,
    window: &MinimalSoftwareWindow,
    fb: &mut FrameBuffer,
    i2c: &RefCell<I2cBus>,
    mut store: Store,
    mut feed_watchdog: impl FnMut(),
) -> ! {
    let tags = Rc::new(VecModel::<TagChoice>::default());
    let request = Rc::new(Cell::new(Request::None));
    connect_callbacks(app, &tags, &request);

    let mut touch = TouchInput::default();
    // The picture on screen, if any.
    let mut showing: Option<Picture> = None;
    let mut shown_at = Instant::now();
    // Change picture as soon as one is ready (new tags were chosen).
    let mut change_now = false;
    let mut menu_open = false;
    let mut last_touch = Instant::now();
    // After a tap opens the menu, ignore that finger until it lifts, so it
    // can't press a button on the menu.
    let mut ignore_touch = false;
    let mut seen_version = u32::MAX;
    // The minute the clock on screen shows, if it's showing.
    let mut clock_on_screen: Option<u32> = None;

    loop {
        feed_watchdog();

        // Show changes to the Wi-Fi and fetcher status, and the tag list.
        let version = state::with(|s| s.version);
        if version != seen_version {
            seen_version = version;
            state::with(|s| {
                app.set_wifi(s.wifi.as_str().into());
                app.set_status(s.pictures.as_str().into());
            });
            if menu_open {
                refresh_tags(app, &tags, false);
            }
        }

        // Touch goes to Slint when it's on screen; on a picture, a tap
        // opens the menu.
        let slint_on_screen = menu_open || showing.is_none();
        match gt911::poll(&mut *i2c.borrow_mut()) {
            Ok(t) => {
                if matches!(t, gt911::Touch::Pressed { .. }) {
                    last_touch = Instant::now();
                }
                if ignore_touch {
                    ignore_touch = !matches!(t, gt911::Touch::Released);
                } else if slint_on_screen {
                    touch.handle(window, t);
                } else if matches!(t, gt911::Touch::Pressed { .. }) {
                    request.set(Request::OpenMenu);
                    ignore_touch = true;
                }
            }
            Err(e) => warn!("touch: {e:?}"),
        }

        match request.take() {
            Request::None => {}
            Request::OpenMenu if !menu_open => {
                menu_open = true;
                last_touch = Instant::now();
                refresh_tags(app, &tags, true);
                app.set_menu_open(true);
            }
            Request::OpenMenu => {}
            Request::Done => {
                let chosen: Vec<String> = tags
                    .iter()
                    .filter(|t| t.selected)
                    .map(|t| t.name.as_str().into())
                    .collect();
                info!("tags chosen: {chosen:?}");
                let before = state::generation();
                state::select_tags(chosen);
                let tags_changed = state::generation() != before;
                let show_phrase = app.get_show_phrase();
                let phrase_changed = state::with(|s| core::mem::replace(&mut s.show_phrase, show_phrase)) != show_phrase;
                let show_clock = app.get_show_clock();
                let clock_changed = state::with(|s| core::mem::replace(&mut s.show_clock, show_clock)) != show_clock;
                // Show the next picture straight away with the new choice.
                // The picture's name makes room for the clock, so it needs
                // laying out again when the clock comes or goes.
                if tags_changed || phrase_changed || (clock_changed && show_phrase) {
                    change_now = true;
                }
                if tags_changed || phrase_changed || clock_changed {
                    let choices = Choices {
                        tags: state::with(|s| s.selected_tags.clone()),
                        show_phrase,
                        show_clock,
                    };
                    save_blanked(fb, &mut store, &choices).await;
                    // The screen below is redrawn as the menu closes.
                    window.request_redraw();
                }
                menu_open = false;
            }
        }
        if menu_open && last_touch.elapsed() >= MENU_TIMEOUT {
            info!("menu timed out");
            menu_open = false;
        }
        if !menu_open && app.get_menu_open() {
            // The menu just closed: put the picture back, or the status page.
            app.set_menu_open(false);
            if let Some(picture) = &showing {
                clock_on_screen = clock_minute();
                show_still(fb, picture, clock_on_screen).await;
            }
        }

        // Time for the next picture?
        let due = showing.is_none() || change_now || shown_at.elapsed() >= SHOW_TIME;
        if !menu_open && due {
            if let Ok(next) = NEXT_PICTURE.try_receive() {
                if next.generation == state::generation() {
                    info!("showing {}", next.phrase);
                    let mut picture = next.picture;
                    let (show_phrase, show_clock) = state::with(|s| (s.show_phrase, s.show_clock));
                    if show_phrase {
                        picture.draw_caption(&next.phrase, show_clock);
                    }
                    let from = showing
                        .take()
                        .unwrap_or_else(|| Picture::blank(slideshow::BACKGROUND));
                    clock_on_screen = clock_minute();
                    show(fb, &from, &picture, clock_on_screen).await;
                    showing = Some(picture);
                    shown_at = Instant::now();
                    change_now = false;
                }
                // Otherwise it was chosen for the old tags: drop it.
                PICTURE_TAKEN.signal(());
            }
        }

        // Keep the clock up to date, and put it up or take it down when
        // it's turned on or off.
        if !menu_open && let Some(picture) = &showing {
            let minute = clock_minute();
            if minute != clock_on_screen {
                clock_on_screen = minute;
                show_still(fb, picture, minute).await;
            }
        }

        // Draw the Slint screens, and wait before polling again.
        if menu_open || showing.is_none() {
            slint::platform::update_timers_and_animations();
            if platform::render(window, fb) {
                wait_presented(fb).await;
            }
            let idle = if window.has_active_animations() {
                Duration::from_millis(1)
            } else {
                slint::platform::duration_until_next_timer_update()
                    .map(|d| Duration::from_micros(d.as_micros() as u64))
                    .unwrap_or(Duration::from_millis(16))
                    .clamp(Duration::from_millis(1), Duration::from_millis(16))
            };
            Timer::after(idle).await;
        } else {
            Timer::after(PICTURE_POLL).await;
        }
    }
}

fn connect_callbacks(app: &AppWindow, tags: &Rc<VecModel<TagChoice>>, request: &Rc<Cell<Request>>) {
    app.set_tags(ModelRc::from(tags.clone()));

    let r = request.clone();
    app.on_open_menu(move || r.set(Request::OpenMenu));
    let r = request.clone();
    app.on_done(move || r.set(Request::Done));

    let (model, weak) = (tags.clone(), app.as_weak());
    app.on_toggle_tag(move |i| {
        let i = i as usize;
        if let Some(mut tag) = model.row_data(i) {
            tag.selected = !tag.selected;
            model.set_row_data(i, tag);
        }
        if let Some(app) = weak.upgrade() {
            app.set_all_selected(!model.iter().any(|t| t.selected));
        }
    });

    let (model, weak) = (tags.clone(), app.as_weak());
    app.on_show_all(move || {
        for i in 0..model.row_count() {
            if let Some(mut tag) = model.row_data(i).filter(|t| t.selected) {
                tag.selected = false;
                model.set_row_data(i, tag);
            }
        }
        if let Some(app) = weak.upgrade() {
            app.set_all_selected(true);
        }
    });
}

/// Fills the menu's tag list from the server's tags. `reset` starts from
/// the tags now in use; otherwise the choices made in the menu so far are
/// kept.
fn refresh_tags(app: &AppWindow, model: &VecModel<TagChoice>, reset: bool) {
    let chosen: Vec<String> = if reset {
        state::with(|s| s.selected_tags.clone())
    } else {
        model
            .iter()
            .filter(|t| t.selected)
            .map(|t| t.name.as_str().into())
            .collect()
    };
    let server_tags = state::with(|s| s.server_tags.clone());
    app.set_tags_loaded(server_tags.is_some());
    if reset {
        let (show_phrase, show_clock) = state::with(|s| (s.show_phrase, s.show_clock));
        app.set_show_phrase(show_phrase);
        app.set_show_clock(show_clock);
    }

    let mut names = server_tags.unwrap_or_default();
    // Keep chosen tags even if the server no longer lists them, so they can
    // be turned off.
    for tag in &chosen {
        if !names.contains(tag) {
            names.push(tag.clone());
        }
    }
    let rows: Vec<TagChoice> = names
        .into_iter()
        .map(|name| TagChoice {
            selected: chosen.contains(&name),
            name: name.into(),
        })
        .collect();
    if !reset && rows.len() == model.row_count() && rows.iter().zip(model.iter()).all(|(a, b)| *a == b) {
        return;
    }
    model.set_vec(rows);
    app.set_all_selected(chosen.is_empty());
}

/// The minute for the clock to show, or `None` if it's off or the time
/// isn't known yet.
fn clock_minute() -> Option<u32> {
    if state::with(|s| s.show_clock) { clock::minute_of_day() } else { None }
}

/// Puts `picture` on screen, with the clock showing `clock` over it.
async fn show_still(fb: &mut FrameBuffer, picture: &Picture, clock: Option<u32>) {
    fb.pixels().copy_from_slice(picture.pixels());
    if let Some(minute) = clock {
        clock::draw(fb.pixels(), &clock::text(minute));
    }
    fb.present();
    wait_presented(fb).await;
}

/// Replaces `from` on screen with `to`, using the configured transition,
/// with the clock showing `clock` over both.
async fn show(fb: &mut FrameBuffer, from: &Picture, to: &Picture, clock: Option<u32>) {
    if slideshow::TRANSITION == Transition::Cut || slideshow::FADE_MILLISECONDS == 0 {
        show_still(fb, to, clock).await;
        return;
    }
    let clock_text = clock.map(clock::text);

    // Each step shows the mix due at the time it starts, so a slow step
    // skips ahead rather than making the fade longer.
    let start = Instant::now();
    let mut frames = 0;
    let mut last = u32::MAX;
    loop {
        let elapsed = start.elapsed().as_millis();
        let amount = (elapsed * FADE_STEPS as u64 / slideshow::FADE_MILLISECONDS).min(FADE_STEPS as u64) as u32;
        if amount != last {
            picture::blend(from.pixels(), to.pixels(), fb.pixels(), amount);
            if let Some(text) = &clock_text {
                clock::draw(fb.pixels(), text);
            }
            fb.present();
            wait_presented(fb).await;
            frames += 1;
            last = amount;
        } else {
            Timer::after(Duration::from_millis(1)).await;
        }
        if amount == FADE_STEPS {
            break;
        }
    }
    info!("fade: {frames} steps in {} ms", start.elapsed().as_millis());
}

/// Saves `choices` with the screen black. Writing flash stops the display
/// refill for about 40 ms, and the panel keeps showing whatever is left in
/// the bounce buffers, drifting out of line until the next VSYNC. When all of
/// it is black, none of that shows.
async fn save_blanked(fb: &mut FrameBuffer, store: &mut Store, choices: &Choices) {
    // Twice, so that one whole black frame has been scanned out and the
    // bounce buffers hold nothing else.
    for _ in 0..2 {
        fb.pixels().fill(0);
        fb.present();
        wait_presented(fb).await;
    }
    store.save(choices);
}

/// Waits until the last frame presented is on screen, so the back buffer
/// is free to draw into.
async fn wait_presented(fb: &FrameBuffer) {
    while !fb.is_presented() {
        Timer::after(Duration::from_millis(1)).await;
    }
}
