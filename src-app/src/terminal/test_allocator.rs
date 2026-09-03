//! The one allocator the `paneflow` test binary installs.
//!
//! A crate may register a single `#[global_allocator]`, so every test that
//! needs to observe allocations shares this wrapper. It forwards each call to
//! the system allocator unchanged and records two things around it: the
//! largest single allocation the current thread has requested (the Kitty
//! decoder test proves a refused PNG never reached its pixel buffer), and the
//! process-wide bytes and call counts (`perf_bench` reports them per
//! iteration, and they are exact where timings are not).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    /// Largest single allocation the current thread has requested since the
    /// last [`reset_largest_allocation`].
    static LARGEST_ALLOCATION: Cell<usize> = const { Cell::new(0) };
}

static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);

/// Test-only allocator: records each thread's largest allocation and counts
/// every allocation in the process.
struct RecordingAllocator;

impl RecordingAllocator {
    fn record(size: usize, growth: usize) {
        let _ = LARGEST_ALLOCATION.try_with(|largest| largest.set(largest.get().max(size)));
        ALLOCATED_BYTES.fetch_add(growth as u64, Ordering::Relaxed);
        ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to `System` unchanged; the recorder only
// touches a thread-local `Cell` and two relaxed atomics, which never allocate.
unsafe impl GlobalAlloc for RecordingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record(layout.size(), layout.size());
        // SAFETY: the caller's obligations are forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::record(layout.size(), layout.size());
        // SAFETY: the caller's obligations are forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's obligations are forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record(new_size, new_size.saturating_sub(layout.size()));
        // SAFETY: the caller's obligations are forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: RecordingAllocator = RecordingAllocator;

/// Forget the current thread's largest allocation so a test can measure
/// only what it is about to do.
pub(crate) fn reset_largest_allocation() {
    LARGEST_ALLOCATION.with(|largest| largest.set(0));
}

/// Largest single allocation the current thread requested since the last
/// [`reset_largest_allocation`].
pub(crate) fn largest_allocation() -> usize {
    LARGEST_ALLOCATION.with(Cell::get)
}

/// Process-wide `(bytes, calls)` allocated so far. Bytes count the requested
/// size of every allocation and the growth of every reallocation; frees are
/// not subtracted, so a difference between two reads is the allocation
/// volume in between.
pub(crate) fn allocation_counters() -> (u64, u64) {
    (
        ALLOCATED_BYTES.load(Ordering::Relaxed),
        ALLOCATION_CALLS.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_grow_with_an_allocation_and_the_largest_is_per_thread() {
        reset_largest_allocation();
        let (bytes_before, calls_before) = allocation_counters();
        let buffer = vec![0u8; 4096];
        let (bytes_after, calls_after) = allocation_counters();
        assert!(
            bytes_after - bytes_before >= 4096,
            "{bytes_before} -> {bytes_after}"
        );
        assert!(calls_after > calls_before);
        assert!(largest_allocation() >= 4096);
        drop(buffer);
        reset_largest_allocation();
        assert_eq!(largest_allocation(), 0);
    }
}
