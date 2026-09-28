//! State shared between the Wi-Fi task, the picture fetcher, the decoder
//! and the display loop.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Instant;

use crate::config::{clock, slideshow};
use crate::picture::Picture;

pub struct Shared {
    /// Tags chosen on the frame. Empty means every picture.
    pub selected_tags: Vec<String>,
    /// Bumped whenever `selected_tags` changes, so that pictures chosen for
    /// the old tags can be recognised and dropped.
    pub generation: u32,
    /// Write each picture's phrase on it.
    pub show_phrase: bool,
    /// Show the clock over the pictures.
    pub show_clock: bool,
    /// UTC in milliseconds since 1970 at an [`Instant`], from the time
    /// server; `None` until it has answered (see [`crate::clock`]).
    pub utc: Option<(Instant, u64)>,
    /// Every tag on the server; `None` until the list has been fetched.
    pub server_tags: Option<Vec<String>>,
    /// What the Wi-Fi is doing, for the status screen.
    pub wifi: String,
    /// What the picture fetcher is doing, or its last problem.
    pub pictures: String,
    /// Bumped on every change, so the UI knows when to refresh.
    pub version: u32,
}

static SHARED: Mutex<CriticalSectionRawMutex, RefCell<Shared>> = Mutex::new(RefCell::new(Shared {
    selected_tags: Vec::new(),
    generation: 0,
    show_phrase: slideshow::SHOW_PHRASE,
    show_clock: clock::SHOW,
    utc: None,
    server_tags: None,
    wifi: String::new(),
    pictures: String::new(),
    version: 0,
}));

/// A picture ready to show.
pub struct Prepared {
    pub picture: Picture,
    pub phrase: String,
    /// [`Shared::generation`] when the picture was chosen.
    pub generation: u32,
}

/// The fetcher's next picture. Holding one means one is always ready when
/// it's time to change.
pub static NEXT_PICTURE: Channel<CriticalSectionRawMutex, Prepared, 1> = Channel::new();

/// Wakes the fetcher early, when the tags change while it is waiting.
pub static WAKE_FETCHER: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Raised by the display loop when it has finished with a picture from
/// [`NEXT_PICTURE`] (faded it in, or dropped it as out of date). The fetcher
/// waits for it before starting the next download, so downloading and
/// decoding never compete with a fade for PSRAM bandwidth.
pub static PICTURE_TAKEN: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// JPEGs for the decoder ([`crate::fetcher::decoder_task`]).
pub static DECODE_JOBS: Channel<CriticalSectionRawMutex, Vec<u8>, 1> = Channel::new();

/// The decoder's results: `None` if a JPEG couldn't be decoded.
pub static DECODED: Channel<CriticalSectionRawMutex, Option<Picture>, 1> = Channel::new();

/// Reads or changes the shared state. Keep `f` short: interrupts are off
/// while it runs.
pub fn with<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    SHARED.lock(|shared| f(&mut shared.borrow_mut()))
}

pub fn generation() -> u32 {
    with(|s| s.generation)
}

pub fn set_wifi_status(status: impl Into<String>) {
    let status = status.into();
    with(|s| {
        s.wifi = status;
        s.version = s.version.wrapping_add(1);
    });
}

pub fn set_picture_status(status: impl Into<String>) {
    let status = status.into();
    with(|s| {
        if s.pictures != status {
            s.pictures = status;
            s.version = s.version.wrapping_add(1);
        }
    });
}

/// Chooses which tags to show, and starts over with pictures for them.
pub fn select_tags(tags: Vec<String>) {
    with(|s| {
        if s.selected_tags != tags {
            s.selected_tags = tags;
            s.generation = s.generation.wrapping_add(1);
            s.version = s.version.wrapping_add(1);
        }
    });
    WAKE_FETCHER.signal(());
}
