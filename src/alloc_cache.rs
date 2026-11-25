//! Global allocator wrapper on Windows that caches recently freed large video-frame
//! buffers (512 KiB ..= 32 MiB) instead of immediately returning them to the OS.
//!
//! Windows `HeapAlloc` forwards allocations >= 512 KiB directly to `NtAllocateVirtualMemory` /
//! `NtFreeVirtualMemory`. Allocating and freeing an 8.3 MB 1080p frame 25-60 times per second
//! triggers thousands of kernel zero-page faults and cross-core IPI TLB shootdowns per frame,
//! causing DPC/audio-thread latency spikes on USB audio devices.

#[cfg(target_os = "windows")]
mod imp {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};

    const MIN_CACHE_SIZE: usize = 512 * 1024;
    const MAX_CACHE_SIZE: usize = 32 * 1024 * 1024;
    const SLOTS: usize = 8;

    #[derive(Clone, Copy)]
    struct Slot {
        ptr: *mut u8,
        size: usize,
        align: usize,
    }

    impl Slot {
        const EMPTY: Self = Self { ptr: ptr::null_mut(), size: 0, align: 0 };
    }

    // SAFETY: pointers in Slot are owned heap allocations returned from System::alloc
    // and only accessed while holding the spinlock.
    unsafe impl Send for Slot {}

    struct Pool {
        slots: [Slot; SLOTS],
        evict_cursor: usize,
    }

    static LOCK: AtomicBool = AtomicBool::new(false);
    static mut POOL: Pool = Pool { slots: [Slot::EMPTY; SLOTS], evict_cursor: 0 };

    #[inline]
    fn lock() -> PoolGuard {
        while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        PoolGuard
    }

    struct PoolGuard;
    impl Drop for PoolGuard {
        #[inline]
        fn drop(&mut self) {
            LOCK.store(false, Ordering::Release);
        }
    }

    #[inline]
    fn cacheable(layout: Layout) -> bool {
        let size = layout.size();
        (MIN_CACHE_SIZE..=MAX_CACHE_SIZE).contains(&size)
    }

    fn pop_cached(layout: Layout) -> *mut u8 {
        let _g = lock();
        // SAFETY: protected by LOCK; no allocations occur inside this critical section.
        let pool = unsafe { &mut *ptr::addr_of_mut!(POOL) };
        for slot in &mut pool.slots {
            if !slot.ptr.is_null() && slot.size == layout.size() && slot.align == layout.align() {
                let p = slot.ptr;
                *slot = Slot::EMPTY;
                return p;
            }
        }
        ptr::null_mut()
    }

    fn push_cached(ptr: *mut u8, layout: Layout) -> Option<Slot> {
        let _g = lock();
        // SAFETY: protected by LOCK; no allocations occur inside this critical section.
        let pool = unsafe { &mut *ptr::addr_of_mut!(POOL) };
        for slot in &mut pool.slots {
            if slot.ptr.is_null() {
                *slot = Slot { ptr, size: layout.size(), align: layout.align() };
                return None;
            }
        }
        let idx = pool.evict_cursor;
        pool.evict_cursor = (idx + 1) % SLOTS;
        let evicted = pool.slots[idx];
        pool.slots[idx] = Slot { ptr, size: layout.size(), align: layout.align() };
        Some(evicted)
    }

    pub struct LargeBufferAllocator;

    unsafe impl GlobalAlloc for LargeBufferAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if cacheable(layout) {
                let cached = pop_cached(layout);
                if !cached.is_null() {
                    return cached;
                }
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if cacheable(layout) {
                let cached = pop_cached(layout);
                if !cached.is_null() {
                    unsafe { ptr::write_bytes(cached, 0, layout.size()) };
                    return cached;
                }
            }
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if !ptr.is_null() && cacheable(layout) {
                if let Some(evicted) = push_cached(ptr, layout) {
                    if !evicted.ptr.is_null() {
                        // SAFETY: evicted came from System::alloc with this exact size and align.
                        let evicted_layout = unsafe { Layout::from_size_align_unchecked(evicted.size, evicted.align) };
                        unsafe { System.dealloc(evicted.ptr, evicted_layout) };
                    }
                }
                return;
            }
            unsafe { System.dealloc(ptr, layout) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if new_size == layout.size() {
                return ptr;
            }
            // SAFETY: every pointer returned by alloc/alloc_zeroed originally came from System::alloc
            // with `layout`, so System::realloc is valid directly.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    /// Releases any cached large buffers back to the OS (e.g. when stopping playback).
    pub fn purge() {
        let drained = {
            let _g = lock();
            // SAFETY: protected by LOCK.
            let pool = unsafe { &mut *ptr::addr_of_mut!(POOL) };
            let drained = pool.slots;
            pool.slots = [Slot::EMPTY; SLOTS];
            drained
        };
        for slot in drained {
            if !slot.ptr.is_null() {
                // SAFETY: slot came from System::alloc with this exact size and align.
                let layout = unsafe { Layout::from_size_align_unchecked(slot.size, slot.align) };
                unsafe { System.dealloc(slot.ptr, layout) };
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::{LargeBufferAllocator, purge};

#[cfg(not(target_os = "windows"))]
pub fn purge() {}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    #[test]
    fn reuses_1080p_frame_buffer_without_reallocating() {
        let bytes = 1920 * 1080 * 4;
        let mut first = Vec::<u8>::with_capacity(bytes);
        first.push(1);
        let first_ptr = first.as_ptr() as usize;
        assert_eq!(first_ptr & 63, 0);
        drop(first);

        let mut second = Vec::<u8>::with_capacity(bytes);
        second.push(2);
        assert_eq!(second.as_ptr() as usize, first_ptr);
        drop(second);

        super::purge();
    }
}
