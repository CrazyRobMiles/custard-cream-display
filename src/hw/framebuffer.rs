//! An RGB565 framebuffer in PSRAM, scanned out to the panel through bounce
//! buffers in internal SRAM.
//!
//! The LCD_CAM must be fed at a fixed 32 MB/s (16 MHz x 2 bytes). PSRAM shares
//! its bus with flash, so when the DMA reads the framebuffer straight from
//! PSRAM, any CPU activity (drawing, heap access, code fetches from flash)
//! can starve it. The FIFO then underruns and the picture loses horizontal
//! sync. This is the same scheme as ESP-IDF's `bounce_buffer_size_px`:
//!
//! - The DMA only ever reads two small buffers in internal SRAM, which it
//!   can always read at full speed. They are linked in a ring.
//! - When the DMA finishes one buffer, the EOF interrupt refills it with the
//!   next part of the frame from PSRAM. It has a whole buffer's scan-out time
//!   (about 0.5 ms) to do so.
//! - At each VSYNC the ring is stopped, reloaded with the top of the frame
//!   and restarted, so scan-out stays aligned with the panel.
//!
//! There are two framebuffers: the CPU draws into one while the other is
//! scanned out, and they swap at VSYNC. The CPU draws through the data cache
//! and the interrupt reads through the same cache, so no flushing is needed.

use core::slice;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use esp_hal::dma::{
    BurstConfig, DmaDescriptor, DmaTxBuffer, ExternalBurstConfig, InternalBurstConfig, Owner,
    Preparation,
};
use esp_hal::interrupt::{self, Priority};
use esp_hal::peripherals::{DMA, Interrupt, LCD_CAM};
use esp_hal::{handler, ram};
use static_cell::ConstStaticCell;

use crate::config::display::{BOUNCE_LINES, DESCRIPTOR_LINES, HEIGHT, WIDTH};

pub const BYTES_PER_LINE: usize = WIDTH * 2;
pub const SIZE: usize = BYTES_PER_LINE * HEIGHT;

/// Alignment of the framebuffer in PSRAM.
pub const ALIGN: usize = 64;

/// The GDMA channel passed to `Dpi::new`.
pub const DMA_CHANNEL: usize = 2;

/// Lines per bounce buffer (chosen by build.rs to suit the panel size).
const LINES_PER_BOUNCE: usize = BOUNCE_LINES;
const BOUNCE_BYTES: usize = BYTES_PER_LINE * LINES_PER_BOUNCE;
const BOUNCE_WORDS: usize = BOUNCE_BYTES / 4;
const CHUNKS_PER_FRAME: usize = HEIGHT / LINES_PER_BOUNCE;

/// Lines per descriptor, keeping under the 4095-byte limit.
const LINES_PER_DESCRIPTOR: usize = DESCRIPTOR_LINES;
const DESCRIPTOR_BYTES: usize = BYTES_PER_LINE * LINES_PER_DESCRIPTOR;
const DESCRIPTORS_PER_BOUNCE: usize = LINES_PER_BOUNCE / LINES_PER_DESCRIPTOR;
const DESCRIPTOR_COUNT: usize = 2 * DESCRIPTORS_PER_BOUNCE;

const _: () = assert!(HEIGHT % LINES_PER_BOUNCE == 0);
const _: () = assert!(LINES_PER_BOUNCE % LINES_PER_DESCRIPTOR == 0);
const _: () = assert!(DESCRIPTOR_BYTES <= 4095);

static DESCRIPTORS: ConstStaticCell<[DmaDescriptor; DESCRIPTOR_COUNT]> =
    ConstStaticCell::new([DmaDescriptor::EMPTY; DESCRIPTOR_COUNT]);

/// The two bounce buffers, in internal SRAM.
static mut BOUNCE: [[u32; BOUNCE_WORDS]; 2] = [[0; BOUNCE_WORDS]; 2];

// State shared with the interrupt handlers.
/// The front buffer: the one being scanned out.
static PIXELS: AtomicUsize = AtomicUsize::new(0);
/// A finished back buffer waiting to become the front buffer at the next
/// VSYNC, or 0.
static PENDING: AtomicUsize = AtomicUsize::new(0);
static RING_HEAD: AtomicUsize = AtomicUsize::new(0);
static NEXT_CHUNK: AtomicUsize = AtomicUsize::new(0);
static NEXT_BOUNCE: AtomicUsize = AtomicUsize::new(0);

/// PSRAM needed: two framebuffers (front and back).
pub const TOTAL_SIZE: usize = 2 * SIZE;

/// Double-buffered: the CPU draws into the back buffer while the front
/// buffer is scanned out, and [`present`](Self::present) swaps them at the
/// next VSYNC, so the panel never shows a half-drawn frame.
pub struct FrameBuffer {
    descriptors: &'static mut [DmaDescriptor; DESCRIPTOR_COUNT],
    buffers: [*mut u16; 2],
    back: usize,
}

// The raw pointers refer to PSRAM that this struct owns exclusively.
unsafe impl Send for FrameBuffer {}

impl FrameBuffer {
    /// # Safety
    ///
    /// `memory` must point to at least [`TOTAL_SIZE`] bytes of PSRAM, aligned
    /// to [`ALIGN`], that nothing else uses for the rest of the program.
    pub unsafe fn new(memory: *mut u8) -> Self {
        assert!(memory as usize % ALIGN == 0);
        let descriptors = DESCRIPTORS.take();
        let first: *mut DmaDescriptor = descriptors.as_mut_ptr();
        let bounce = (&raw mut BOUNCE).cast::<u8>();
        for (i, desc) in descriptors.iter_mut().enumerate() {
            desc.set_size(DESCRIPTOR_BYTES);
            desc.set_length(DESCRIPTOR_BYTES);
            desc.set_owner(Owner::Dma);
            // EOF on the last descriptor of each bounce buffer raises the
            // interrupt that refills it.
            desc.set_suc_eof(i % DESCRIPTORS_PER_BOUNCE == DESCRIPTORS_PER_BOUNCE - 1);
            // Bounce buffer 0 then 1, contiguous in memory.
            desc.buffer = unsafe { bounce.add(i * DESCRIPTOR_BYTES) };
            desc.next = unsafe { first.add((i + 1) % DESCRIPTOR_COUNT) };
        }
        let buffers = [memory.cast::<u16>(), unsafe { memory.add(SIZE) }.cast::<u16>()];
        PIXELS.store(buffers[0] as usize, Ordering::Relaxed);
        RING_HEAD.store(first as usize, Ordering::Relaxed);

        let fb = Self {
            descriptors,
            buffers,
            back: 1,
        };
        load_top_of_frame();
        fb
    }

    /// The back buffer, to draw the next frame into.
    ///
    /// After [`present`](Self::present), wait for
    /// [`is_presented`](Self::is_presented) before drawing again.
    pub fn pixels(&mut self) -> &mut [u16] {
        unsafe { slice::from_raw_parts_mut(self.buffers[self.back], WIDTH * HEIGHT) }
    }

    /// Shows the back buffer from the next frame on. The old front buffer
    /// becomes the back buffer once [`is_presented`](Self::is_presented).
    pub fn present(&mut self) {
        PENDING.store(self.buffers[self.back] as usize, Ordering::Release);
        self.back ^= 1;
    }

    /// True once the last [`present`](Self::present) has taken effect, so the
    /// back buffer is no longer on screen.
    pub fn is_presented(&self) -> bool {
        PENDING.load(Ordering::Acquire) == 0
    }


    /// Starts refilling the bounce buffers from the framebuffer.
    ///
    /// Call once the DPI transfer is running on [`DMA_CHANNEL`].
    pub fn start_scanout(&self) {
        interrupt::bind_handler(Interrupt::DMA_OUT_CH2, on_bounce_done);
        interrupt::bind_handler(Interrupt::LCD_CAM, on_vsync);
        let ch = DMA::regs().ch(DMA_CHANNEL);
        ch.out_int().clr().write(|w| w.out_eof().clear_bit_by_one());
        ch.out_int().ena().modify(|_, w| w.out_eof().set_bit());
        LCD_CAM::regs()
            .lc_dma_int_ena()
            .modify(|_, w| w.lcd_vsync_int_ena().set_bit());
    }
}

/// DBG (wifi_test only): refill the bounce buffers with a fixed pattern
/// instead of reading the framebuffer, so the interrupt never touches PSRAM.
pub static TEST_SKIP_PSRAM: AtomicBool = AtomicBool::new(false);

/// Copies chunk `chunk` of the frame into bounce buffer `bounce`.
///
/// Runs from RAM with volatile word copies so the compiler can't turn it
/// into a `memcpy` call in flash.
#[ram]
fn fill(bounce: usize, chunk: usize) {
    if TEST_SKIP_PSRAM.load(Ordering::Relaxed) {
        unsafe {
            let dst = (&raw mut BOUNCE[bounce]).cast::<u32>();
            // Same number of stores; a dark blue that varies with the chunk.
            let value = 0x0008_0008 * (chunk as u32 & 3);
            let mut i = 0;
            while i < BOUNCE_WORDS {
                dst.add(i).write_volatile(value);
                i += 1;
            }
        }
        return;
    }
    unsafe {
        let src = (PIXELS.load(Ordering::Relaxed) as *const u32).add(chunk * BOUNCE_WORDS);
        let dst = (&raw mut BOUNCE[bounce]).cast::<u32>();
        // Eight words per iteration: loads are issued back to back, so cache
        // line fills from PSRAM overlap better.
        let mut i = 0;
        while i < BOUNCE_WORDS {
            let a = src.add(i).read_volatile();
            let b = src.add(i + 1).read_volatile();
            let c = src.add(i + 2).read_volatile();
            let d = src.add(i + 3).read_volatile();
            let e = src.add(i + 4).read_volatile();
            let f = src.add(i + 5).read_volatile();
            let g = src.add(i + 6).read_volatile();
            let h = src.add(i + 7).read_volatile();
            dst.add(i).write_volatile(a);
            dst.add(i + 1).write_volatile(b);
            dst.add(i + 2).write_volatile(c);
            dst.add(i + 3).write_volatile(d);
            dst.add(i + 4).write_volatile(e);
            dst.add(i + 5).write_volatile(f);
            dst.add(i + 6).write_volatile(g);
            dst.add(i + 7).write_volatile(h);
            i += 8;
        }
    }
}

const _: () = assert!(BOUNCE_WORDS % 8 == 0);

/// Puts the first two chunks of the frame in the bounce buffers.
#[ram]
fn load_top_of_frame() {
    fill(0, 0);
    fill(1, 1);
    NEXT_CHUNK.store(2, Ordering::Relaxed);
    NEXT_BOUNCE.store(0, Ordering::Relaxed);
}

/// The DMA has read a whole bounce buffer; refill it with the next chunk.
#[handler(priority = Priority::Priority3)]
#[ram]
fn on_bounce_done() {
    DMA::regs()
        .ch(DMA_CHANNEL)
        .out_int()
        .clr()
        .write(|w| w.out_eof().clear_bit_by_one());

    let bounce = NEXT_BOUNCE.load(Ordering::Relaxed);
    let chunk = NEXT_CHUNK.load(Ordering::Relaxed);
    // Past the bottom of the frame the DMA only prefetches data the panel
    // won't use; VSYNC reloads the top of the frame.
    if chunk < CHUNKS_PER_FRAME {
        // On the 800x480 panel this takes ~320 us (PSRAM reads at ~50 MB/s);
        // the deadline is the ~510 us it takes to scan out the other buffer.
        fill(bounce, chunk);
    }
    NEXT_CHUNK.store(chunk + 1, Ordering::Relaxed);
    NEXT_BOUNCE.store(bounce ^ 1, Ordering::Relaxed);
}

/// End of frame: restart the ring from the top of the frame during blanking.
#[handler(priority = Priority::Priority3)]
#[ram]
fn on_vsync() {
    let lcd = LCD_CAM::regs();
    lcd.lc_dma_int_clr()
        .write(|w| w.lcd_vsync_int_clr().set_bit());

    let ch = DMA::regs().ch(DMA_CHANNEL);
    ch.out_link().modify(|_, w| w.outlink_stop().set_bit());
    ch.out_conf0().modify(|_, w| w.out_rst().set_bit());
    ch.out_conf0().modify(|_, w| w.out_rst().clear_bit());

    // Swap in a newly finished frame, if there is one.
    let pending = PENDING.load(Ordering::Acquire);
    if pending != 0 {
        PIXELS.store(pending, Ordering::Relaxed);
        PENDING.store(0, Ordering::Release);
    }

    // Blanking is short (the pulse and back porch are only ~12 lines), so load
    // just the first buffer, restart the DMA, and fill the second buffer
    // while the first one is being scanned out.
    fill(0, 0);
    ch.out_int().clr().write(|w| w.out_eof().clear_bit_by_one());

    // Drop whatever the FIFO prefetched from the end of the last frame.
    lcd.lcd_misc().modify(|_, w| w.lcd_afifo_reset().set_bit());
    ch.out_link()
        .modify(|_, w| unsafe { w.outlink_addr().bits(RING_HEAD.load(Ordering::Relaxed) as u32) });
    ch.out_link().modify(|_, w| w.outlink_start().set_bit());

    fill(1, 1);
    NEXT_CHUNK.store(2, Ordering::Relaxed);
    NEXT_BOUNCE.store(0, Ordering::Relaxed);
}

unsafe impl DmaTxBuffer for FrameBuffer {
    type View = FrameBuffer;
    type Final = FrameBuffer;

    fn prepare(&mut self) -> Preparation {
        Preparation {
            start: self.descriptors.as_mut_ptr(),
            accesses_psram: false,
            burst_transfer: BurstConfig {
                external_memory: ExternalBurstConfig::Size64,
                internal_memory: InternalBurstConfig::Enabled,
            },
            // The ring is never handed back to the CPU, so the owner bits stay
            // as initialised and must not be checked.
            check_owner: Some(false),
            auto_write_back: false,
        }
    }

    fn into_view(self) -> Self::View {
        self
    }

    fn from_view(view: Self::View) -> Self::Final {
        view
    }
}
