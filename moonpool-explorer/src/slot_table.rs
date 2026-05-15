//! Shared bump-allocator primitive for fixed-capacity slot tables.
//!
//! Used by `assertion_slots` and `each_buckets`. The atomic counter
//! lives at offset 0 of the shared-memory region; slot entries follow
//! at offset 8 (or wherever the caller chooses to map them).
//!
//! Memory ordering is left to the caller because the two consumers
//! have different visibility requirements: assertion slots need
//! Acquire/AcqRel to publish slot claims across processes, whereas
//! each-buckets is single-writer-per-region and uses Relaxed.

use std::sync::atomic::{AtomicU32, Ordering};

/// Read the current slot count from the table's counter word.
///
/// # Safety
///
/// `table_ptr` must point to a valid region whose first 4 bytes are
/// an `AtomicU32` slot count.
pub(crate) unsafe fn load_slot_count(table_ptr: *mut u8, ordering: Ordering) -> usize {
    unsafe {
        let next_atomic = &*(table_ptr as *const AtomicU32);
        next_atomic.load(ordering) as usize
    }
}

/// Atomically bump-allocate a new slot index, returning `Some(idx)` if
/// the table has capacity or `None` if it is full (the counter is
/// rolled back in that case).
///
/// # Safety
///
/// `table_ptr` must point to a valid region whose first 4 bytes are
/// an `AtomicU32` slot count.
pub(crate) unsafe fn try_bump_alloc(
    table_ptr: *mut u8,
    max_slots: usize,
    ordering: Ordering,
) -> Option<usize> {
    unsafe {
        let next_atomic = &*(table_ptr as *const AtomicU32);
        let new_idx = next_atomic.fetch_add(1, ordering) as usize;
        if new_idx >= max_slots {
            next_atomic.fetch_sub(1, ordering);
            return None;
        }
        Some(new_idx)
    }
}
