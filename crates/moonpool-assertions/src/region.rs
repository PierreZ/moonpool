//! Storage for the assertion + each-bucket regions.
//!
//! By default the regions are heap-allocated by [`init`] — portable everywhere
//! (wasm, macOS, Linux), single process, no sharing. An exploration backend that
//! needs cross-`fork` sharing calls [`install_region`] with `MAP_SHARED` pointers
//! it owns; this crate then accounts into that memory and [`clear`]s its view when
//! the backend frees it.
//!
//! Pointers are thread-locals set before forking, so forked children inherit them
//! (the `MAP_SHARED` region is the same physical memory in parent and child).

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::cell::Cell;

use crate::buckets::EACH_BUCKET_MEM_SIZE;
use crate::slots::ASSERTION_TABLE_MEM_SIZE;

/// Alignment for both regions. `AssertionSlot`/`EachBucket` contain `u64`/`i64`
/// fields and the layout places the slot array at offset 8, so 8-byte alignment
/// satisfies every field access.
const REGION_ALIGN: usize = 8;

thread_local! {
    static ASSERTION_TABLE: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };
    static EACH_BUCKET_PTR: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };
    /// True when the current pointers are heap regions this crate allocated (and
    /// must free); false when an external backend installed them (it frees).
    static HEAP_OWNED: Cell<bool> = const { Cell::new(false) };
}

/// Layout of a region of `size` bytes.
fn layout(size: usize) -> Layout {
    // Infallible: size is a compile-time constant and align is a power of two.
    Layout::from_size_align(size, REGION_ALIGN)
        .expect("region layout: const size, power-of-two align")
}

/// Both regions with their sizes: the assertion table, then the each-buckets.
fn regions() -> [(*mut u8, usize); 2] {
    [
        (assertion_table_ptr(), ASSERTION_TABLE_MEM_SIZE),
        (each_bucket_ptr(), EACH_BUCKET_MEM_SIZE),
    ]
}

fn set_regions(table: *mut u8, buckets: *mut u8, heap_owned: bool) {
    ASSERTION_TABLE.set(table);
    EACH_BUCKET_PTR.set(buckets);
    HEAP_OWNED.set(heap_owned);
}

/// Get the raw pointer to the assertion table region (null if uninitialized).
#[must_use]
pub fn assertion_table_ptr() -> *mut u8 {
    ASSERTION_TABLE.get()
}

/// Get the raw pointer to the each-bucket region (null if uninitialized).
#[must_use]
pub fn each_bucket_ptr() -> *mut u8 {
    EACH_BUCKET_PTR.get()
}

/// Allocate the regions on the heap (zeroed). Idempotent: a no-op if a region is
/// already present (whether heap-owned or installed by a backend).
pub fn init() {
    if !assertion_table_ptr().is_null() {
        return;
    }
    let alloc = |size| {
        // Safety: layouts have non-zero size; alloc_zeroed returns zeroed memory
        // of the requested size/alignment, matching the
        // `[count: u32, header: u32, entries..]` layout.
        let region = unsafe { alloc_zeroed(layout(size)) };
        if region.is_null() {
            std::alloc::handle_alloc_error(layout(size));
        }
        region
    };
    set_regions(
        alloc(ASSERTION_TABLE_MEM_SIZE),
        alloc(EACH_BUCKET_MEM_SIZE),
        true,
    );
}

/// Point accounting at caller-owned regions (e.g. `MAP_SHARED` memory from an
/// exploration backend). The caller owns the memory and is responsible for
/// freeing it after calling [`clear`]. Frees any heap regions this crate
/// previously allocated.
///
/// # Safety
///
/// Both pointers must be non-null, zero-initialized, aligned to at least eight
/// bytes, reference at least [`crate::ASSERTION_TABLE_MEM_SIZE`] and
/// [`crate::EACH_BUCKET_MEM_SIZE`] bytes respectively, and stay valid until
/// [`clear`] is called. The regions must not be reset or freed while accounting
/// calls may access them.
pub unsafe fn install_region(table: *mut u8, buckets: *mut u8) {
    free_heap_regions();
    set_regions(table, buckets, false);
}

/// Drop this crate's view of the regions. Frees heap regions it owns; for
/// installed (external) regions it only nulls the pointers — the backend frees
/// its own memory.
pub fn clear() {
    free_heap_regions();
    set_regions(std::ptr::null_mut(), std::ptr::null_mut(), false);
}

fn free_heap_regions() {
    if !HEAP_OWNED.get() {
        return;
    }
    for (region, size) in regions() {
        if !region.is_null() {
            // Safety: heap-owned regions were allocated by init() with layout(size).
            unsafe { dealloc(region, layout(size)) };
        }
    }
    HEAP_OWNED.set(false);
}

/// Zero both regions for a between-run reset. No-op if not initialized.
pub fn reset() {
    for (region, size) in regions() {
        if !region.is_null() {
            // Safety: the region is `size` bytes (heap or installed).
            unsafe { std::ptr::write_bytes(region, 0, size) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::{AssertKind, assertion_bool, assertion_read_all};

    #[test]
    fn heap_init_enables_accounting_and_reset() {
        // Each nextest test runs in its own process, so this thread starts with
        // null regions.
        assert!(assertion_table_ptr().is_null());
        init();
        assert!(!assertion_table_ptr().is_null());

        // Accounting works on the heap region.
        assertion_bool(AssertKind::Sometimes, true, true, "site_a");
        let slots = assertion_read_all();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].pass_count, 1);

        // reset() zeroes counts.
        reset();
        assert!(assertion_read_all().is_empty());

        clear();
        assert!(assertion_table_ptr().is_null());
    }
}
