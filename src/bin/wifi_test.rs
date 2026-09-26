//! Wi-Fi soak test: no display at all. Joins Wi-Fi like the frame does, then
//! keeps asking the server for pictures and logs how every request went.
//!
//! It uses the frame's own network code (`network`, `http`, `server`), but
//! never runs Slint, touch or the slideshow. Set [`SECOND_CORE`] and
//! [`SCANOUT`] to compare runs with and without the second core and the
//! panel scan-out.
//!
//! Flash and watch it with:
//!
//! ```text
//! cargo run --release --bin wifi_test
//! ```

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use core::sync::atomic::{AtomicU32, Ordering};

use custard_cream_display::config::{logical_size, server};
use custard_cream_display::hw::display;
use custard_cream_display::hw::framebuffer::{self, FrameBuffer};
use custard_cream_display::picture::Picture;
use custard_cream_display::{heap, network, server as api};
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::lcd_cam::LcdCam;
use esp_hal::lcd_cam::lcd::dpi::Dpi;
use esp_hal::psram::{Psram, PsramConfig, PsramMode, SpiRamFreq};
use esp_hal::rng::Rng;
use esp_hal::system::Stack as CoreStack;
use esp_hal::timer::timg::TimerGroup;
use log::{info, warn};
use static_cell::{ConstStaticCell, StaticCell};

extern crate alloc;
use alloc::vec::Vec;

esp_bootloader_esp_idf::esp_app_desc!();

/// What the second core does during the test.
#[allow(dead_code)]
#[derive(PartialEq, Eq, Debug)]
enum SecondCore {
    /// Not started: everything runs on core 0.
    Off,
    /// Started, with an executor that has nothing to do.
    Idle,
    /// Decodes every downloaded JPEG, as the frame does.
    Decode,
    /// Not started; each JPEG is decoded on core 0 instead.
    DecodeOnCore0,
}

/// Change these to compare runs.
const SECOND_CORE: SecondCore = SecondCore::DecodeOnCore0;
/// Run the RGB panel scan-out as the frame does: DMA from bounce buffers
/// refilled from the PSRAM framebuffer by interrupts on core 0. The
/// backlight stays off and nothing is drawn.
const SCANOUT: bool = true;
/// With [`SCANOUT`], refill the bounce buffers with a fixed pattern instead
/// of reading the framebuffer: same interrupts, but no PSRAM reads in them.
const SCANOUT_SKIPS_PSRAM: bool = false;

/// Pause between picture downloads (the frame fetches one every 20 s).
const INTERVAL: Duration = Duration::from_secs(5);

/// How often to print a summary.
const SUMMARY_EVERY: Duration = Duration::from_secs(30);

static OK: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);
static DECODED: AtomicU32 = AtomicU32::new(0);
/// Seconds since boot of the last request that finished, either way.
static LAST_DONE: AtomicU32 = AtomicU32::new(0);

/// JPEGs for the second core in [`SecondCore::Decode`].
static JOBS: Channel<CriticalSectionRawMutex, Vec<u8>, 1> = Channel::new();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    info!(
        "wifi_test: second core {SECOND_CORE:?}, scan-out {SCANOUT}, skip PSRAM {SCANOUT_SKIPS_PSRAM}, a picture every {} s",
        INTERVAL.as_secs()
    );
    info!("Reset reason: {:?}", esp_hal::system::reset_reason());

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Same heap layout as the frame, minus the framebuffers.
    let psram = Psram::new(
        peripherals.PSRAM,
        PsramConfig {
            mode: PsramMode::OctalSpi,
            ram_frequency: SpiRamFreq::Freq80m,
            ..Default::default()
        },
    );
    let (psram_start, psram_size) = psram.raw_parts();
    // Framebuffers first, as in the frame, whether or not they're used.
    let fb = unsafe { FrameBuffer::new(psram_start) };
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    esp_alloc::heap_allocator!(size: 96 * 1024);
    unsafe {
        heap::add_psram(
            psram_start.add(framebuffer::TOTAL_SIZE),
            psram_size - framebuffer::TOTAL_SIZE,
        )
    };

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    if matches!(SECOND_CORE, SecondCore::Idle | SecondCore::Decode) {
        static CORE1_STACK: ConstStaticCell<CoreStack<81920>> = ConstStaticCell::new(CoreStack::new());
        esp_rtos::start_second_core(
            peripherals.CPU_CTRL,
            peripherals.FROM_CPU_INTR1,
            CORE1_STACK.take(),
            || {
                static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();
                EXECUTOR.init(esp_rtos::embassy::Executor::new()).run(|spawner| {
                    if SECOND_CORE == SecondCore::Decode {
                        spawner.spawn(decoder_task().unwrap());
                    }
                });
            },
        );
    }

    let wifi = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_tx_queue_size(8),
    )
    .expect("Wi-Fi init");
    let seed = {
        let rng = Rng::new();
        (rng.random() as u64) << 32 | rng.random() as u64
    };
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        esp_radio::wifi::Interface::station(),
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    info!("Server: {}", server::URL);

    spawner.spawn(network::net_task(runner).unwrap());
    spawner.spawn(network::wifi_task(wifi, stack).unwrap());
    spawner.spawn(summary_task().unwrap());

    // Kept alive for the rest of the program.
    let _transfer = if SCANOUT {
        let lcd_cam = LcdCam::new(peripherals.LCD_CAM);
        let dpi = Dpi::new(lcd_cam.lcd, peripherals.DMA_CH2, display::dpi_config())
            .unwrap()
            .with_vsync(peripherals.GPIO3)
            .with_hsync(peripherals.GPIO46)
            .with_de(peripherals.GPIO5)
            .with_pclk(peripherals.GPIO7)
            .with_data0(peripherals.GPIO14)
            .with_data1(peripherals.GPIO38)
            .with_data2(peripherals.GPIO18)
            .with_data3(peripherals.GPIO17)
            .with_data4(peripherals.GPIO10)
            .with_data5(peripherals.GPIO39)
            .with_data6(peripherals.GPIO0)
            .with_data7(peripherals.GPIO45)
            .with_data8(peripherals.GPIO48)
            .with_data9(peripherals.GPIO47)
            .with_data10(peripherals.GPIO21)
            .with_data11(peripherals.GPIO1)
            .with_data12(peripherals.GPIO2)
            .with_data13(peripherals.GPIO42)
            .with_data14(peripherals.GPIO41)
            .with_data15(peripherals.GPIO40);
        framebuffer::TEST_SKIP_PSRAM.store(SCANOUT_SKIPS_PSRAM, core::sync::atomic::Ordering::Relaxed);
        let transfer = dpi.send(true, fb).map_err(|e| e.0).unwrap();
        transfer.start_scanout();
        info!("Scan-out running");
        Some(transfer)
    } else {
        None
    };

    fetch_forever(stack).await
}

/// Asks for a batch of pictures, downloads them one by one, repeats.
async fn fetch_forever(stack: Stack<'static>) -> ! {
    let (width, height) = logical_size();
    let size = width.max(height);
    loop {
        stack.wait_config_up().await;

        let started = Instant::now();
        let batch = api::random_batch(stack, &[]).await;
        let phrases = match batch {
            Ok(phrases) => {
                done(true);
                info!("batch: {} pictures in {} ms", phrases.len(), started.elapsed().as_millis());
                phrases
            }
            Err(e) => {
                done(false);
                warn!("batch: {e} after {} ms", started.elapsed().as_millis());
                Timer::after(INTERVAL).await;
                continue;
            }
        };

        for phrase in phrases {
            Timer::after(INTERVAL).await;
            let started = Instant::now();
            match api::picture(stack, &phrase, size).await {
                Ok(jpeg) => {
                    done(true);
                    info!("{phrase}: {} KB in {} ms", jpeg.len() / 1024, started.elapsed().as_millis());
                    match SECOND_CORE {
                        SecondCore::Decode => JOBS.send(jpeg).await,
                        SecondCore::DecodeOnCore0 => decode(&jpeg),
                        _ => {}
                    }
                }
                Err(e) => {
                    done(false);
                    warn!("{phrase}: {e} after {} ms", started.elapsed().as_millis());
                }
            }
        }
    }
}

fn done(ok: bool) {
    if ok { &OK } else { &FAILED }.fetch_add(1, Ordering::Relaxed);
    LAST_DONE.store(Instant::now().as_secs() as u32, Ordering::Relaxed);
}

/// Prints the counts regularly. It keeps going if requests get stuck, so a
/// silent log means core 0 itself has stopped.
#[embassy_executor::task]
async fn summary_task() {
    loop {
        Timer::after(SUMMARY_EVERY).await;
        let now = Instant::now().as_secs() as u32;
        info!(
            "SUMMARY up {} s: {} ok, {} failed, {} decoded, last request finished {} s ago, \
             tx in flight {}, heap {} KB internal, {} KB PSRAM",
            now,
            OK.load(Ordering::Relaxed),
            FAILED.load(Ordering::Relaxed),
            DECODED.load(Ordering::Relaxed),
            now.saturating_sub(LAST_DONE.load(Ordering::Relaxed)),
            esp_radio::wifi::tx_in_flight(),
            heap::free_internal() / 1024,
            heap::free_psram() / 1024,
        );
    }
}

/// Second core, [`SecondCore::Decode`]: decodes each JPEG and throws it away.
#[embassy_executor::task]
async fn decoder_task() {
    loop {
        let jpeg = JOBS.receive().await;
        decode(&jpeg);
    }
}

/// Decodes a JPEG and throws the picture away.
fn decode(jpeg: &[u8]) {
    let started = Instant::now();
    match Picture::from_jpeg(jpeg, &mut Vec::new()) {
        Ok(_) => {
            DECODED.fetch_add(1, Ordering::Relaxed);
            info!("decoded in {} ms", started.elapsed().as_millis());
        }
        Err(e) => warn!("decode: {e:?}"),
    }
}
