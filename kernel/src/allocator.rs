use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

// MMIO address to report current heap usage in bytes
const MMIO_MEM_USAGE: *mut u64 = 0x2000_0000 as *mut u64;

unsafe extern "C" {
    static mut _sheap: u8;
    static mut _eheap: u8;
}

static HEAP_PTR: AtomicUsize = AtomicUsize::new(0);
static HEAP_START: AtomicUsize = AtomicUsize::new(0);
static HEAP_END: AtomicUsize = AtomicUsize::new(0);

pub fn init() {
    unsafe {
        let start = _sheap as *const u8 as usize;
        let end = _eheap as *const u8 as usize;
        HEAP_START.store(start, Ordering::Relaxed);
        HEAP_END.store(end, Ordering::Relaxed);
        HEAP_PTR.store(start, Ordering::Relaxed);
        // Report initial usage (0)
        core::ptr::write_volatile(MMIO_MEM_USAGE, 0);
    }
}

pub struct BumpAllocator;

#[inline]
fn align_up(addr: usize, align: usize) -> usize {
    let mask = align - 1;
    (addr + mask) & !mask
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let start = HEAP_START.load(Ordering::Relaxed);
        let end = HEAP_END.load(Ordering::Relaxed);

        loop {
            let current = HEAP_PTR.load(Ordering::Relaxed);
            let aligned = align_up(current, layout.align());
            let next = match aligned.checked_add(layout.size()) {
                Some(n) => n,
                None => return core::ptr::null_mut(),
            };
            if next > end {
                return core::ptr::null_mut();
            }
            if HEAP_PTR
                .compare_exchange(current, next, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                // Report new usage
                let used = (next - start) as u64;
                core::ptr::write_volatile(MMIO_MEM_USAGE, used);
                return aligned as *mut u8;
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // No-op: simple bump allocator does not support free.
        // Optionally, could report here as well, but we keep usage monotonic.
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: BumpAllocator = BumpAllocator;
