//! custard-cream-display: start-up. Brings up PSRAM, Wi-Fi and the display
//! hardware, starts the network and picture-fetching tasks, then runs the
//! display loop ([`frame::run`]).

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use alloc::string::String;
use core::cell::RefCell;

use custard_cream_display::config::{self, DisplayType, server, slideshow};
use custard_cream_display::hw::board::exio;
use custard_cream_display::hw::ch422g::Ch422g;
use custard_cream_display::hw::framebuffer::{self, FrameBuffer};
use custard_cream_display::hw::{I2cBus, display};
use custard_cream_display::ui::AppWindow;
use custard_cream_display::store::Store;
use custard_cream_display::{clock, fetcher, frame, heap, network, platform, state};
use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::lcd_cam::LcdCam;
use esp_hal::lcd_cam::lcd::dpi::Dpi;
use esp_hal::psram::{Psram, PsramConfig, PsramMode, SpiRamFreq};
use esp_hal::rng::Rng;
use esp_hal::time::Rate;
use esp_hal::timer::timg::{MwdtStage, MwdtStageAction, TimerGroup};
use log::info;
use slint::ComponentHandle;
use static_cell::StaticCell;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    info!("custard-cream-display {}", env!("CARGO_PKG_VERSION"));
    info!("Reset reason: {:?}", esp_hal::system::reset_reason());

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let psram = Psram::new(
        peripherals.PSRAM,
        PsramConfig {
            mode: PsramMode::OctalSpi,
            ram_frequency: SpiRamFreq::Freq80m,
            ..Default::default()
        },
    );
    let (psram_start, psram_size) = psram.raw_parts();
    info!("PSRAM: {} KB", psram_size / 1024);
    // The framebuffers take the start of PSRAM; the rest becomes heap.
    let fb = unsafe { FrameBuffer::new(psram_start) };

    // Small allocations (and the Wi-Fi driver's) use internal RAM; big
    // ones use PSRAM. See custard_cream_display::heap.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    // Includes the decoder thread's 64 KB stack.
    esp_alloc::heap_allocator!(size: 160 * 1024);
    unsafe {
        heap::add_psram(
            psram_start.add(framebuffer::TOTAL_SIZE),
            psram_size - framebuffer::TOTAL_SIZE,
        );
    }

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    // Above the decoder thread, so the display loop always comes first.
    esp_rtos::CurrentThreadHandle::get().set_priority(fetcher::MAIN_TASK_PRIORITY);

    // Everything runs on the first core. With the display's scan-out
    // interrupts on one core and JPEG decoding on the other, the Wi-Fi
    // driver stopped working within minutes (see src/bin/wifi_test.rs);
    // on one core it doesn't. Decoding happens while a picture is showing,
    // in a low-priority thread (fetcher::start_decoder).

    // Wi-Fi first, while there is plenty of internal RAM.
    // The driver lets only `tx_queue_size` frames be in flight at once; the
    // default of 3 is tight for a download's ACKs alongside DNS and DHCP.
    let wifi = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_tx_queue_size(8),
    )
    .expect("Wi-Fi init");
    let station = esp_radio::wifi::Interface::station();
    let seed = {
        let rng = Rng::new();
        (rng.random() as u64) << 32 | rng.random() as u64
    };
    // Sockets: DHCP, DNS, the fetcher's TCP and the clock's UDP.
    static RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        station,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    info!("Server: {}", server::URL);

    info!(
        "Heap free: {} KB internal, {} KB PSRAM",
        heap::free_internal() / 1024,
        heap::free_psram() / 1024
    );

    // Start with the menu choices saved last time, or else the settings.
    let mut store = Store::new(peripherals.FLASH);
    match store.load() {
        Some(choices) => {
            info!("Saved choices: {choices:?}");
            state::with(|s| {
                s.show_phrase = choices.show_phrase;
                s.show_clock = choices.show_clock;
            });
            state::select_tags(choices.tags);
        }
        None => state::select_tags(slideshow::TAGS.iter().map(|&t| String::from(t)).collect()),
    }
    spawner.spawn(network::net_task(runner).unwrap());
    spawner.spawn(network::wifi_task(wifi, stack).unwrap());
    spawner.spawn(fetcher::fetcher_task(stack).unwrap());
    spawner.spawn(clock::sntp_task(stack).unwrap());
    fetcher::start_decoder();

    // Display hardware. Only one type so far; a new board adds its own
    // bring-up here.
    match config::display::TYPE {
        DisplayType::WaveshareEsp32S3TouchLcd7 => {}
    }

    static I2C: StaticCell<RefCell<I2cBus>> = StaticCell::new();
    let i2c: &'static RefCell<I2cBus> = I2C.init(RefCell::new(
        I2c::new(
            peripherals.I2C0,
            I2cConfig::default().with_frequency(Rate::from_khz(400)),
        )
        .unwrap()
        .with_sda(peripherals.GPIO8)
        .with_scl(peripherals.GPIO9),
    ));

    // Everything starts low: LCD and touch in reset, backlight off.
    static EXPANDER: StaticCell<Ch422g<'static, I2cBus>> = StaticCell::new();
    let expander: &'static Ch422g<'static, I2cBus> =
        EXPANDER.init(Ch422g::new(i2c, 1 << exio::SD_CS).unwrap());

    // GT911 reset: INT held low while reset is released selects address 0x5D.
    let mut tp_int = peripherals.GPIO4;
    {
        let _int = Output::new(tp_int.reborrow(), Level::Low, OutputConfig::default());
        Timer::after(Duration::from_millis(10)).await;
        expander.set(exio::TP_RST, true).unwrap();
        Timer::after(Duration::from_millis(60)).await;
    }
    let _tp_int = Input::new(tp_int, InputConfig::default());

    expander.set(exio::LCD_RST, true).unwrap();
    Timer::after(Duration::from_millis(100)).await;

    // UI
    let window = platform::init();
    let app = AppWindow::new().unwrap();
    app.show().unwrap();

    // Draw the status page, start scanning out, then light the panel.
    let mut fb = fb;
    slint::platform::update_timers_and_animations();
    platform::render(&window, &mut fb);
    let lcd_cam = LcdCam::new(peripherals.LCD_CAM);
    let dpi = Dpi::new(lcd_cam.lcd, peripherals.DMA_CH2, display::dpi_config())
        .unwrap()
        .with_vsync(peripherals.GPIO3)
        .with_hsync(peripherals.GPIO46)
        .with_de(peripherals.GPIO5)
        .with_pclk(peripherals.GPIO7)
        // Blue
        .with_data0(peripherals.GPIO14)
        .with_data1(peripherals.GPIO38)
        .with_data2(peripherals.GPIO18)
        .with_data3(peripherals.GPIO17)
        .with_data4(peripherals.GPIO10)
        // Green
        .with_data5(peripherals.GPIO39)
        .with_data6(peripherals.GPIO0)
        .with_data7(peripherals.GPIO45)
        .with_data8(peripherals.GPIO48)
        .with_data9(peripherals.GPIO47)
        .with_data10(peripherals.GPIO21)
        // Red
        .with_data11(peripherals.GPIO1)
        .with_data12(peripherals.GPIO2)
        .with_data13(peripherals.GPIO42)
        .with_data14(peripherals.GPIO41)
        .with_data15(peripherals.GPIO40);
    let mut transfer = dpi.send(true, fb).map_err(|e| e.0).unwrap();
    transfer.start_scanout();
    expander.set(exio::LCD_BL, true).unwrap();
    info!("Display running");

    // If the display loop stops for this long, restart the whole frame.
    // This works even if the core is stuck with interrupts off.
    let mut watchdog = TimerGroup::new(peripherals.TIMG1).wdt;
    watchdog.set_timeout(MwdtStage::Stage0, esp_hal::time::Duration::from_secs(15));
    watchdog.set_stage_action(MwdtStage::Stage0, MwdtStageAction::ResetSystem);
    watchdog.enable();

    frame::run(&app, &window, &mut transfer, i2c, store, || watchdog.feed()).await
}
