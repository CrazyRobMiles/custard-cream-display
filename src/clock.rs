//! The clock in the bottom-right corner of the screen (`[clock] show`, or
//! "Show clock" in the menu).
//!
//! The frame has no battery-backed clock, so it asks a time server (SNTP)
//! once it is on the network, and again every few hours. Local time comes
//! from `[clock] utc_offset_minutes` and `daylight_saving`. Until the first
//! answer arrives there is no clock to show. Integer arithmetic only (see
//! Cargo.toml).

use alloc::format;
use alloc::string::String;

use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use log::{info, warn};

use crate::caption;
use crate::config::{ClockFormat, DaylightSaving, clock};
use crate::state;

/// How often to ask the time server again. The ESP32-S3's crystal keeps
/// the clock within a second or so over this long.
const RESYNC: Duration = Duration::from_secs(6 * 60 * 60);
/// Wait before trying again after the server didn't answer.
const RETRY: Duration = Duration::from_secs(30);
/// How long to wait for the server's reply.
const TIMEOUT: Duration = Duration::from_secs(5);

const NTP_PORT: u16 = 123;
/// Seconds from the start of 1900 (NTP's epoch) to the start of 1970.
const NTP_TO_UNIX: u64 = 2_208_988_800;

const MINUTES_PER_DAY: u32 = 24 * 60;
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// Keeps the time in [`state::Shared::utc`] up to date.
#[embassy_executor::task]
pub async fn sntp_task(stack: Stack<'static>) {
    loop {
        stack.wait_config_up().await;
        match ask_time_server(stack).await {
            Ok(unix_ms) => {
                info!("clock set from {}: {} s after 1970", clock::NTP_SERVER, unix_ms / 1000);
                state::with(|s| s.utc = Some((Instant::now(), unix_ms)));
                Timer::after(RESYNC).await;
            }
            Err(e) => {
                warn!("clock: {e}");
                Timer::after(RETRY).await;
            }
        }
    }
}

/// Asks the time server for the time, in milliseconds since 1970 (UTC).
async fn ask_time_server(stack: Stack<'_>) -> Result<u64, &'static str> {
    let address = *stack
        .dns_query(clock::NTP_SERVER, DnsQueryType::A)
        .await
        .map_err(|_| "can't find the time server")?
        .first()
        .ok_or("can't find the time server")?;

    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx = [0u8; 64];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx = [0u8; 64];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.bind(0).map_err(|_| "can't open a socket")?;

    // An SNTP request: version 3, client mode, everything else zero.
    let mut packet = [0u8; 48];
    packet[0] = 0x1B;
    socket
        .send_to(&packet, (address, NTP_PORT))
        .await
        .map_err(|_| "can't send to the time server")?;
    let (length, _) = with_timeout(TIMEOUT, socket.recv_from(&mut packet))
        .await
        .map_err(|_| "no answer from the time server")?
        .map_err(|_| "bad answer from the time server")?;
    // Server mode, and a stratum other than 0 ("kiss of death").
    if length < 48 || packet[0] & 0x07 != 4 || packet[1] == 0 {
        return Err("bad answer from the time server");
    }

    // The transmit timestamp: seconds since 1900, then a 32-bit fraction.
    let seconds = u32::from_be_bytes([packet[40], packet[41], packet[42], packet[43]]) as u64;
    let fraction = u32::from_be_bytes([packet[44], packet[45], packet[46], packet[47]]) as u64;
    // The seconds wrap in 2036; small values are after that.
    let seconds = if seconds < 1 << 31 { seconds + (1 << 32) } else { seconds };
    Ok((seconds - NTP_TO_UNIX) * 1000 + (fraction * 1000 >> 32))
}

/// The local time as minutes since midnight, or `None` until the time
/// server has answered.
pub fn minute_of_day() -> Option<u32> {
    let (at, unix_ms) = state::with(|s| s.utc)?;
    let utc = ((unix_ms + at.elapsed().as_millis()) / 1000) as i64;
    let mut local = utc + clock::UTC_OFFSET_MINUTES as i64 * 60;
    if summer_time(utc) {
        local += 60 * 60;
    }
    Some((local.rem_euclid(SECONDS_PER_DAY) / 60) as u32)
}

/// The clock's text for `minute_of_day`.
pub fn text(minute_of_day: u32) -> String {
    let (hour, minute) = (minute_of_day / 60 % 24, minute_of_day % 60);
    match clock::FORMAT {
        ClockFormat::TwentyFourHour => format!("{hour:02}:{minute:02}"),
        ClockFormat::TwelveHour => {
            let suffix = if hour < 12 { "am" } else { "pm" };
            let hour = if hour % 12 == 0 { 12 } else { hour % 12 };
            format!("{hour}:{minute:02} {suffix}")
        }
    }
}

/// The width of the clock's panel at its widest (see [`caption::panel_width`]).
pub fn panel_width() -> i32 {
    caption::panel_width(&text(MINUTES_PER_DAY - 1))
}

/// Draws the clock showing `text` into a screenful of panel-order pixels.
pub fn draw(pixels: &mut [u16], text: &str) {
    caption::draw(pixels, text, caption::Corner::Right, i32::MAX);
}

/// Whether summer time is in force at `utc` (seconds since 1970).
fn summer_time(utc: i64) -> bool {
    match clock::DAYLIGHT_SAVING {
        DaylightSaving::None => false,
        // From 01:00 UTC on the last Sunday in March to 01:00 UTC on the
        // last Sunday in October.
        DaylightSaving::Europe => {
            let (year, _, _) = civil_from_days(utc.div_euclid(SECONDS_PER_DAY));
            let start = last_sunday(year, 3) * SECONDS_PER_DAY + 60 * 60;
            let end = last_sunday(year, 10) * SECONDS_PER_DAY + 60 * 60;
            (start..end).contains(&utc)
        }
        // From 02:00 local standard time on the second Sunday in March to
        // 02:00 local summer time (01:00 standard) on the first Sunday in
        // November.
        DaylightSaving::UnitedStates => {
            let standard = utc + clock::UTC_OFFSET_MINUTES as i64 * 60;
            let (year, _, _) = civil_from_days(standard.div_euclid(SECONDS_PER_DAY));
            let start = nth_sunday(year, 3, 2) * SECONDS_PER_DAY + 2 * 60 * 60;
            let end = nth_sunday(year, 11, 1) * SECONDS_PER_DAY + 60 * 60;
            (start..end).contains(&standard)
        }
    }
}

/// Day of the week for a day number (days since 1970): 0 is Sunday.
fn weekday(days: i64) -> i64 {
    // 1 January 1970 was a Thursday.
    (days + 4).rem_euclid(7)
}

/// The day number of the `n`th Sunday of `month` in `year`.
fn nth_sunday(year: i64, month: u32, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    first + (7 - weekday(first)) % 7 + 7 * (n - 1)
}

/// The day number of the last Sunday of `month` (not December) in `year`.
fn last_sunday(year: i64, month: u32) -> i64 {
    let last = days_from_civil(year, month + 1, 1) - 1;
    last - weekday(last)
}

// Conversions between dates and day numbers, from Howard Hinnant's
// "chrono-Compatible Low-Level Date Algorithms".

/// Days since 1 January 1970 for a date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date (year, month, day) for a day number.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}
