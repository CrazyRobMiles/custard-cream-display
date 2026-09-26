//! Keeps the next picture ready: asks the server for random pictures with
//! the chosen tags and downloads one. [`decoder_task`] decodes and scales it
//! into a [`Picture`], which then waits in [`state::NEXT_PICTURE`].
//!
//! Decoding an 800x600 picture takes about 2 s. It runs in a low-priority
//! thread ([`start_decoder`]) on the same core as everything else, straight
//! after the previous picture has faded in, so the display loop, touch and
//! the menu carry on as normal while it works.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embassy_futures::select::select;
use embassy_sync::channel::TrySendError;
use embassy_net::Stack;
use embassy_time::{Duration, Timer};
use log::{info, warn};

use crate::config::logical_size;
use crate::http;
use crate::picture::Picture;
use crate::server;
use crate::state::{self, DECODE_JOBS, DECODED, NEXT_PICTURE, PICTURE_TAKEN, Prepared, WAKE_FETCHER};

/// Wait before retrying after the server couldn't be reached.
const RETRY_DELAY: Duration = Duration::from_secs(10);

/// Wait before asking again when no pictures match the tags.
const EMPTY_DELAY: Duration = Duration::from_secs(60);

/// Requests in a row that fail on the network before the frame restarts.
/// Sometimes the Wi-Fi driver stops sending altogether while still
/// reporting a connection. Reconnecting doesn't help (the driver blocks the
/// first core instead), so restart the whole frame.
const FAILURES_BEFORE_REBOOT: u32 = 4;

/// Counts requests in a row that failed on the network, and restarts the
/// frame when it seems stuck. Replies from the server, even errors, show
/// that the network works, so they reset the count.
#[derive(Default)]
struct StuckCheck {
    failures: u32,
}

impl StuckCheck {
    fn check<T>(&mut self, result: &Result<T, http::Error>) {
        use http::Error::{Connect, Dns, Network, Timeout};
        match result {
            Err(Dns | Connect | Network | Timeout) => {
                self.failures += 1;
                if self.failures >= FAILURES_BEFORE_REBOOT {
                    warn!("{} requests in a row failed: restarting", self.failures);
                    esp_hal::system::software_reset();
                }
            }
            _ => self.failures = 0,
        }
    }
}

#[embassy_executor::task]
pub async fn fetcher_task(stack: Stack<'static>) {
    // Ask for pictures big enough to fill the longer side of the screen.
    let (width, height) = logical_size();
    let size = width.max(height);

    let mut queue: Vec<String> = Vec::new();
    let mut queue_generation = u32::MAX;
    let mut stuck = StuckCheck::default();

    loop {
        stack.wait_config_up().await;

        let (generation, tags) = state::with(|s| (s.generation, s.selected_tags.clone()));
        // Any earlier wake-up is covered by the generation just read (at
        // start-up there is always one). Left set, it would end the wait for
        // the display below early, and the fetcher would run a picture
        // ahead: a third picture in PSRAM, and decoding during a fade.
        WAKE_FETCHER.reset();
        if generation != queue_generation {
            queue.clear();
            queue_generation = generation;
        }

        if queue.is_empty() {
            // Refresh the tag list with each batch, so new tags turn up in
            // the menu without a restart.
            let server_tags = server::tags(stack).await;
            stuck.check(&server_tags);
            match server_tags {
                Ok(server_tags) => state::with(|s| {
                    if s.server_tags.as_ref() != Some(&server_tags) {
                        s.server_tags = Some(server_tags);
                        s.version = s.version.wrapping_add(1);
                    }
                }),
                Err(e) => warn!("tag list: {e}"),
            }

            let batch = server::random_batch(stack, &tags).await;
            stuck.check(&batch);
            match batch {
                Ok(batch) if batch.is_empty() => {
                    state::set_picture_status(if tags.is_empty() {
                        "The server has no pictures yet".into()
                    } else {
                        format!("No pictures are tagged {}", tags.join(" or "))
                    });
                    wait_or_wake(EMPTY_DELAY).await;
                    continue;
                }
                Ok(batch) => queue = batch,
                Err(e) => {
                    warn!("picture list: {e}");
                    state::set_picture_status(format!("Can't get pictures: {e}"));
                    wait_or_wake(RETRY_DELAY).await;
                    continue;
                }
            }
        }

        let Some(phrase) = queue.pop() else { continue };
        state::set_picture_status("Loading pictures...");
        let jpeg = server::picture(stack, &phrase, size).await;
        stuck.check(&jpeg);
        let jpeg = match jpeg {
            Ok(jpeg) => jpeg,
            Err(e) => {
                warn!("picture {phrase}: {e}");
                state::set_picture_status(format!("Can't load a picture: {e}"));
                wait_or_wake(RETRY_DELAY).await;
                continue;
            }
        };

        DECODE_JOBS.send(jpeg).await;
        let Some(picture) = DECODED.receive().await else {
            warn!("picture {phrase}: can't decode it");
            continue;
        };

        if state::generation() != generation {
            // The tags changed while this one was on its way.
            continue;
        }
        state::set_picture_status("");
        // Only a signal for this picture may end the wait below.
        PICTURE_TAKEN.reset();
        NEXT_PICTURE
            .send(Prepared {
                picture,
                phrase,
                generation,
            })
            .await;
        // Wait until the display has taken it and finished fading it in.
        select(PICTURE_TAKEN.wait(), WAKE_FETCHER.wait()).await;
    }
}

/// Priority of the decoder thread: below the main task (see
/// [`MAIN_TASK_PRIORITY`]), so it only runs when the display loop, touch
/// and networking have nothing to do, and they interrupt it at once.
const DECODER_PRIORITY: u32 = 0;

/// The main task's priority, which `main` sets. Above the decoder, below
/// the Wi-Fi driver's threads.
pub const MAIN_TASK_PRIORITY: usize = 1;

/// zune-jpeg keeps its Huffman tables (about 30 KB) on the stack; 32 KB
/// overflowed. This comes from the internal RAM heap.
const DECODER_STACK: usize = 64 * 1024;

/// How often the decoder thread looks for work while it has none.
const DECODER_POLL_US: u32 = 20_000;

/// Starts the decoder thread, on the same core as everything else. It
/// decodes each JPEG from [`DECODE_JOBS`] into a screen-ready [`Picture`]
/// and passes it back through [`DECODED`].
///
/// Decoding takes about 2 s, so it runs in a thread of its own at the
/// lowest priority rather than as an async task: the display loop keeps
/// running (touch, the menu) and simply borrows the CPU back whenever it
/// needs it.
pub fn start_decoder() {
    unsafe {
        esp_radio_rtos_driver::task_create(
            "decoder",
            decoder_thread,
            core::ptr::null_mut(),
            DECODER_PRIORITY,
            Some(0),
            DECODER_STACK,
        );
    }
}

extern "C" fn decoder_thread(_: *mut core::ffi::c_void) {
    // Room for the biggest picture the fetcher asks for (it fits inside a
    // square the size of the screen's longer side), allocated once.
    let (width, height) = logical_size();
    let side = width.max(height);
    let mut rgb: Vec<u8> = Vec::with_capacity(side * side * 3);
    loop {
        let jpeg = loop {
            match DECODE_JOBS.try_receive() {
                Ok(jpeg) => break jpeg,
                Err(_) => esp_radio_rtos_driver::usleep(DECODER_POLL_US),
            }
        };
        let started = embassy_time::Instant::now();
        let picture = Picture::from_jpeg(&jpeg, &mut rgb)
            .inspect_err(|e| warn!("decode: {e:?}"))
            .ok();
        drop(jpeg);
        info!("picture ready in {} ms", started.elapsed().as_millis());
        let mut result = picture;
        // The fetcher is waiting for it, so this normally goes straight in.
        while let Err(TrySendError::Full(back)) = DECODED.try_send(result) {
            result = back;
            esp_radio_rtos_driver::usleep(DECODER_POLL_US);
        }
    }
}

/// Waits for `delay`, or less if the tags change.
async fn wait_or_wake(delay: Duration) {
    select(Timer::after(delay), WAKE_FETCHER.wait()).await;
}
