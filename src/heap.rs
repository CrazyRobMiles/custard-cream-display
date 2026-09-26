//! The heap: small allocations in internal RAM, big ones in PSRAM.
//!
//! On the ESP32-S3 the atomic instructions don't work on PSRAM
//! (esp-rs/esp-hal#2027), so anything holding a reference count or a lock
//! must stay in internal RAM. Those are small, while the things that need
//! PSRAM (pictures, JPEGs, downloads) are big, so allocations are split by
//! size, as ESP-IDF does with `SPIRAM_MALLOC_ALWAYSINTERNAL`.
//!
//! [`esp_alloc::HEAP`] holds only internal RAM; it also serves the Wi-Fi
//! driver's own `malloc` calls, which must never land in PSRAM.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

use esp_alloc::{EspHeap, HEAP, HeapRegion, MemoryCapability};

/// Allocations of at least this many bytes go to PSRAM.
pub const PSRAM_THRESHOLD: usize = 16 * 1024;

static PSRAM: EspHeap = EspHeap::empty();
static PSRAM_START: AtomicUsize = AtomicUsize::new(0);
static PSRAM_END: AtomicUsize = AtomicUsize::new(0);

/// Hands `size` bytes of PSRAM at `start` to the big-allocation heap.
///
/// # Safety
///
/// The memory must be PSRAM that nothing else uses, and this must be called
/// once, before anything big is allocated.
pub unsafe fn add_psram(start: *mut u8, size: usize) {
    unsafe {
        PSRAM.add_region(HeapRegion::new(start, size, MemoryCapability::External.into()));
    }
    PSRAM_START.store(start as usize, Ordering::Relaxed);
    PSRAM_END.store(start as usize + size, Ordering::Relaxed);
}

/// Free bytes of internal RAM heap.
pub fn free_internal() -> usize {
    HEAP.free()
}

/// Free bytes of PSRAM heap.
pub fn free_psram() -> usize {
    PSRAM.free()
}

struct SplitHeap;

#[global_allocator]
static ALLOCATOR: SplitHeap = SplitHeap;

unsafe impl GlobalAlloc for SplitHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            if layout.size() >= PSRAM_THRESHOLD {
                let ptr = GlobalAlloc::alloc(&PSRAM, layout);
                if !ptr.is_null() {
                    return ptr;
                }
            }
            // Small allocations never fall back to PSRAM.
            GlobalAlloc::alloc(&HEAP, layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let address = ptr as usize;
        let in_psram = (PSRAM_START.load(Ordering::Relaxed)..PSRAM_END.load(Ordering::Relaxed))
            .contains(&address);
        unsafe {
            if in_psram {
                GlobalAlloc::dealloc(&PSRAM, ptr, layout)
            } else {
                GlobalAlloc::dealloc(&HEAP, ptr, layout)
            }
        }
    }
}
