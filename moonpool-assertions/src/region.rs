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
use std::sync::atomic::{AtomicU32, Ordering};

use crate::buckets::{EACH_BUCKET_MEM_SIZE, EachBucket, MAX_EACH_BUCKETS};
use crate::slots::{ASSERTION_TABLE_MEM_SIZE, AssertionSlot, MAX_ASSERTION_SLOTS};

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

fn table_layout() -> Layout {
    // Infallible: size is a compile-time constant and align is a power of two.
    Layout::from_size_align(ASSERTION_TABLE_MEM_SIZE, REGION_ALIGN)
        .expect("assertion table layout: const size, power-of-two align")
}

fn bucket_layout() -> Layout {
    // Infallible: size is a compile-time constant and align is a power of two.
    Layout::from_size_align(EACH_BUCKET_MEM_SIZE, REGION_ALIGN)
        .expect("each-bucket layout: const size, power-of-two align")
}

/// Get the raw pointer to the assertion table region (null if uninitialized).
#[must_use]
pub fn assertion_table_ptr() -> *mut u8 {
    ASSERTION_TABLE.with(Cell::get)
}

/// Get the raw pointer to the each-bucket region (null if uninitialized).
#[must_use]
pub fn each_bucket_ptr() -> *mut u8 {
    EACH_BUCKET_PTR.with(Cell::get)
}

/// Allocate the regions on the heap (zeroed). Idempotent: a no-op if a region is
/// already present (whether heap-owned or installed by a backend).
pub fn init() {
    if !assertion_table_ptr().is_null() {
        return;
    }
    // Safety: layouts have non-zero size; alloc_zeroed returns zeroed memory of the
    // requested size/alignment, matching the `[count: u32, _pad, slots..]` layout.
    let table = unsafe { alloc_zeroed(table_layout()) };
    if table.is_null() {
        std::alloc::handle_alloc_error(table_layout());
    }
    let buckets = unsafe { alloc_zeroed(bucket_layout()) };
    if buckets.is_null() {
        std::alloc::handle_alloc_error(bucket_layout());
    }
    ASSERTION_TABLE.with(|c| c.set(table));
    EACH_BUCKET_PTR.with(|c| c.set(buckets));
    HEAP_OWNED.with(|c| c.set(true));
}

/// Point accounting at caller-owned regions (e.g. `MAP_SHARED` memory from an
/// exploration backend). The caller owns the memory and is responsible for
/// freeing it after calling [`clear`]. Frees any heap regions this crate
/// previously allocated.
///
/// Both pointers must reference at least [`crate::ASSERTION_TABLE_MEM_SIZE`] /
/// [`crate::EACH_BUCKET_MEM_SIZE`] bytes respectively, and stay valid until
/// [`clear`] is called.
pub fn install_region(table: *mut u8, buckets: *mut u8) {
    free_heap_regions();
    ASSERTION_TABLE.with(|c| c.set(table));
    EACH_BUCKET_PTR.with(|c| c.set(buckets));
    HEAP_OWNED.with(|c| c.set(false));
}

/// Drop this crate's view of the regions. Frees heap regions it owns; for
/// installed (external) regions it only nulls the pointers — the backend frees
/// its own memory.
pub fn clear() {
    free_heap_regions();
    ASSERTION_TABLE.with(|c| c.set(std::ptr::null_mut()));
    EACH_BUCKET_PTR.with(|c| c.set(std::ptr::null_mut()));
}

fn free_heap_regions() {
    if !HEAP_OWNED.with(Cell::get) {
        return;
    }
    let table = assertion_table_ptr();
    if !table.is_null() {
        // Safety: heap-owned table was allocated by init() with table_layout().
        unsafe { dealloc(table, table_layout()) };
    }
    let buckets = each_bucket_ptr();
    if !buckets.is_null() {
        // Safety: heap-owned buckets were allocated by init() with bucket_layout().
        unsafe { dealloc(buckets, bucket_layout()) };
    }
    HEAP_OWNED.with(|c| c.set(false));
}

/// Zero both regions for a between-run reset. No-op if not initialized.
pub fn reset() {
    let table = assertion_table_ptr();
    if !table.is_null() {
        // Safety: region is ASSERTION_TABLE_MEM_SIZE bytes (heap or installed).
        unsafe { std::ptr::write_bytes(table, 0, ASSERTION_TABLE_MEM_SIZE) };
    }
    let buckets = each_bucket_ptr();
    if !buckets.is_null() {
        // Safety: region is EACH_BUCKET_MEM_SIZE bytes (heap or installed).
        unsafe { std::ptr::write_bytes(buckets, 0, EACH_BUCKET_MEM_SIZE) };
    }
}

/// Reset only the per-seed split triggers, preserving pass/fail counts and
/// watermarks/frontiers across seeds (coverage-preserving multi-seed reset).
///
/// `pass_count`/`fail_count` accumulate across seeds so contract validation never
/// reports a false "was never reached" when a seed doesn't reach a guarded
/// assertion. Forking uses `split_triggered`, not counts. No-op if uninitialized.
pub fn prepare_next_seed_reset() {
    let table = assertion_table_ptr();
    if !table.is_null() {
        // Safety: region is the assertion table laid out as
        // `[count: u32, _pad, slots: [AssertionSlot; MAX_ASSERTION_SLOTS]]`.
        unsafe {
            let count_ptr = table.cast::<()>().cast::<AtomicU32>();
            // MAX_ASSERTION_SLOTS is a small const (128), so the cast is lossless.
            let max_slots = u32::try_from(MAX_ASSERTION_SLOTS).unwrap_or(u32::MAX);
            let count = (*count_ptr).load(Ordering::Relaxed).min(max_slots) as usize;
            let base = table.add(8).cast::<()>().cast::<AssertionSlot>();
            for i in 0..count {
                let slot = &mut *base.add(i);
                // Skip tombstones (msg_hash == 0) left by the duplicate-slot race fix.
                if slot.msg_hash == 0 {
                    continue;
                }
                slot.split_triggered = 0;
            }
        }
    }

    let buckets = each_bucket_ptr();
    if !buckets.is_null() {
        // Safety: region is the each-bucket table laid out as
        // `[count: u32, _pad, buckets: [EachBucket; MAX_EACH_BUCKETS]]`.
        unsafe {
            let count_ptr = buckets.cast::<()>().cast::<AtomicU32>();
            // MAX_EACH_BUCKETS is a small const (256), so the cast is lossless.
            let max_buckets = u32::try_from(MAX_EACH_BUCKETS).unwrap_or(u32::MAX);
            let count = (*count_ptr).load(Ordering::Relaxed).min(max_buckets) as usize;
            let base = buckets.add(8).cast::<()>().cast::<EachBucket>();
            for i in 0..count {
                (*base.add(i)).split_triggered = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::{AssertCmp, AssertKind, assertion_bool, assertion_read_all};

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

    #[test]
    fn prepare_next_seed_preserves_counts_resets_triggers() {
        init();
        crate::slots::assertion_numeric(
            AssertKind::NumericSometimes,
            AssertCmp::Gt,
            true,
            5,
            0,
            "n",
        );
        assertion_bool(AssertKind::Sometimes, true, true, "s");
        let before = assertion_read_all();
        let pass_total: u64 = before.iter().map(|s| s.pass_count).sum();
        assert!(pass_total >= 2);

        prepare_next_seed_reset();
        let after = assertion_read_all();
        // Counts preserved across the seed boundary.
        let pass_after: u64 = after.iter().map(|s| s.pass_count).sum();
        assert_eq!(pass_total, pass_after);
        clear();
    }
}
