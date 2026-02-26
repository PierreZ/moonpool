//! LLVM SanitizerCoverage `inline-8bit-counters` integration.
//!
//! When the binary is compiled with `-C instrument-coverage` or
//! `-fsanitize-coverage=inline-8bit-counters`, LLVM inserts a counter
//! increment at every code edge. The linker calls our
//! [`__sanitizer_cov_8bit_counters_init`] callback with the counter
//! array's bounds during static initialization (before `main()`).
//!
//! This module:
//! - Captures the BSS counter array pointer via the LLVM callback
//! - Copies counters to MAP_SHARED memory after each child timeline
//! - Applies AFL-style bucketing to reduce noise
//! - Detects novelty by comparing bucketed counters against a history map
//!
//! All public functions are no-ops when sancov instrumentation is not
//! present (i.e., `COUNTERS_PTR` is null).

use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Global statics — set during static init, before main()
// ---------------------------------------------------------------------------

/// Pointer to the LLVM-generated BSS counter array.
///
/// Set by [`__sanitizer_cov_8bit_counters_init`] during static
/// constructors. Remains null if sancov is not enabled.
static COUNTERS_PTR: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());

/// Number of edges (counters) in the instrumented binary.
static COUNTERS_LEN: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Thread-local state — set during init() from main()
// ---------------------------------------------------------------------------

thread_local! {
    /// MAP_SHARED buffer for child→parent counter transfer.
    ///
    /// Public so `split_loop.rs` can redirect per-child in parallel mode.
    pub static SANCOV_TRANSFER: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };

    /// MAP_SHARED global max map (history of highest bucketed values).
    static SANCOV_HISTORY: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };

    /// MAP_SHARED pool base for parallel mode (one slot per concurrent child).
    ///
    /// Public so `setup_child()` can reset for nested splits.
    pub static SANCOV_POOL: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };

    /// Number of slots in the sancov pool.
    ///
    /// Public so `setup_child()` can reset for nested splits.
    pub static SANCOV_POOL_SLOTS: Cell<usize> = const { Cell::new(0) };
}

// ---------------------------------------------------------------------------
// LLVM callbacks
// ---------------------------------------------------------------------------

/// Called by LLVM during static initialization for each compilation unit.
///
/// Merges ranges via min(start)/max(stop) so multiple TUs are handled.
///
/// # Safety
///
/// `start` and `stop` must point to valid memory. `stop` must be ≥ `start`.
/// This is only called by LLVM instrumentation infrastructure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __sanitizer_cov_8bit_counters_init(start: *mut u8, stop: *mut u8) {
    if start.is_null() || stop.is_null() || stop <= start {
        return;
    }

    // Safety: start and stop are valid pointers provided by LLVM, stop >= start
    let new_len = unsafe { stop.offset_from(start) } as usize;

    // Merge: keep the lowest start and highest stop across all TUs.
    let prev = COUNTERS_PTR.load(Ordering::Relaxed);
    if prev.is_null() || (start as usize) < (prev as usize) {
        COUNTERS_PTR.store(start, Ordering::Relaxed);
    }

    // Accumulate length: the total span is max(stop) - min(start).
    // Since TUs may be non-contiguous, we track the max of all
    // (start + len) and then recompute len as max_stop - min_start.
    let current_start = COUNTERS_PTR.load(Ordering::Relaxed) as usize;
    let new_stop = start as usize + new_len;
    let current_stop = current_start + COUNTERS_LEN.load(Ordering::Relaxed);
    let final_stop = current_stop.max(new_stop);
    COUNTERS_LEN.store(final_stop - current_start, Ordering::Relaxed);
}

/// Called by LLVM for PC table initialization. Stub — we don't use PC info.
///
/// # Safety
///
/// Only called by LLVM instrumentation infrastructure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __sanitizer_cov_pcs_init(_pcs_beg: *const usize, _pcs_end: *const usize) {}

// ---------------------------------------------------------------------------
// AFL bucketing
// ---------------------------------------------------------------------------

/// AFL-style hit-count bucketing table.
///
/// Maps raw edge counts to coarser buckets to reduce noise from
/// minor count variations. The mapping is:
/// - 0 → 0 (not hit)
/// - 1 → 1 (hit once)
/// - 2 → 2 (hit twice)
/// - 3 → 4
/// - 4..=7 → 8
/// - 8..=15 → 16
/// - 16..=31 → 32
/// - 32..=127 → 64
/// - 128..=255 → 128
const COUNT_CLASS_LOOKUP: [u8; 256] = {
    let mut table = [0u8; 256];
    // 0 stays 0
    table[1] = 1;
    table[2] = 2;
    table[3] = 4;
    let mut i = 4;
    while i <= 7 {
        table[i] = 8;
        i += 1;
    }
    i = 8;
    while i <= 15 {
        table[i] = 16;
        i += 1;
    }
    i = 16;
    while i <= 31 {
        table[i] = 32;
        i += 1;
    }
    i = 32;
    while i <= 127 {
        table[i] = 64;
        i += 1;
    }
    i = 128;
    while i <= 255 {
        table[i] = 128;
        i += 1;
    }
    table
};

/// Apply AFL bucketing to a buffer of edge counts in-place.
fn classify_counts(buffer: *mut u8, len: usize) {
    for i in 0..len {
        // Safety: caller ensures buffer has at least `len` bytes
        unsafe {
            let val = *buffer.add(i);
            *buffer.add(i) = COUNT_CLASS_LOOKUP[val as usize];
        }
    }
}

// ---------------------------------------------------------------------------
// Novelty detection
// ---------------------------------------------------------------------------

/// Check for novel coverage in a buffer against a history map.
///
/// Applies AFL bucketing to `buffer` in-place, then compares each
/// bucketed entry against `history`. If `bucketed > history[i]`,
/// updates history and marks novelty found.
///
/// Does NOT early-return: must update all history entries in one pass.
/// Skips zero entries (unvisited edges).
fn has_new_coverage_inner(buffer: *mut u8, history: *mut u8, len: usize) -> bool {
    classify_counts(buffer, len);

    let mut found_new = false;
    for i in 0..len {
        // Safety: caller ensures both buffer and history have at least `len` bytes
        unsafe {
            let bucketed = *buffer.add(i);
            if bucketed == 0 {
                continue;
            }
            let prev = *history.add(i);
            if bucketed > prev {
                *history.add(i) = bucketed;
                found_new = true;
            }
        }
    }
    found_new
}

/// Check for novel sancov coverage in the transfer buffer (sequential path).
///
/// Returns `false` when sancov is unavailable.
pub fn has_new_sancov_coverage() -> bool {
    if !sancov_is_available() {
        return false;
    }
    let transfer = SANCOV_TRANSFER.with(|c| c.get());
    let history = SANCOV_HISTORY.with(|c| c.get());
    if transfer.is_null() || history.is_null() {
        return false;
    }
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    has_new_coverage_inner(transfer, history, len)
}

/// Check for novel sancov coverage from a specific pool slot (parallel path).
///
/// Returns `false` when sancov is unavailable.
pub fn has_new_sancov_coverage_from(slot_ptr: *mut u8) -> bool {
    if !sancov_is_available() || slot_ptr.is_null() {
        return false;
    }
    let history = SANCOV_HISTORY.with(|c| c.get());
    if history.is_null() {
        return false;
    }
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    has_new_coverage_inner(slot_ptr, history, len)
}

// ---------------------------------------------------------------------------
// Public query API
// ---------------------------------------------------------------------------

/// Check if LLVM sancov instrumentation is present.
///
/// Returns `true` when the binary was compiled with sancov and the
/// LLVM callback has registered the counter array.
pub fn sancov_is_available() -> bool {
    !COUNTERS_PTR.load(Ordering::Relaxed).is_null()
}

/// Return the number of instrumented edges.
///
/// Returns 0 when sancov is unavailable.
pub fn sancov_edge_count() -> usize {
    COUNTERS_LEN.load(Ordering::Relaxed)
}

/// Count non-zero entries in the history map (edges ever covered).
///
/// Returns 0 when sancov is unavailable or history is not initialized.
pub fn sancov_edges_covered() -> usize {
    if !sancov_is_available() {
        return 0;
    }
    let history = SANCOV_HISTORY.with(|c| c.get());
    if history.is_null() {
        return 0;
    }
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    let mut count = 0usize;
    for i in 0..len {
        // Safety: history was allocated with at least `len` bytes
        if unsafe { *history.add(i) } != 0 {
            count += 1;
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Initialize sancov shared memory buffers (transfer + history).
///
/// No-op when sancov instrumentation is not available.
///
/// # Errors
///
/// Returns an error if shared memory allocation fails.
pub fn init_sancov_shared() -> Result<(), std::io::Error> {
    if !sancov_is_available() {
        return Ok(());
    }
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    if len == 0 {
        return Ok(());
    }

    let transfer = crate::shared_mem::alloc_shared(len)?;
    let history = crate::shared_mem::alloc_shared(len)?;

    SANCOV_TRANSFER.with(|c| c.set(transfer));
    SANCOV_HISTORY.with(|c| c.set(history));

    Ok(())
}

/// Free sancov shared memory (transfer, history, and pool).
///
/// Nulls all pointers after freeing. No-op if not initialized.
pub fn cleanup_sancov_shared() {
    let len = COUNTERS_LEN.load(Ordering::Relaxed);

    let transfer = SANCOV_TRANSFER.with(|c| c.get());
    if !transfer.is_null() {
        unsafe { crate::shared_mem::free_shared(transfer, len) };
        SANCOV_TRANSFER.with(|c| c.set(std::ptr::null_mut()));
    }

    let history = SANCOV_HISTORY.with(|c| c.get());
    if !history.is_null() {
        unsafe { crate::shared_mem::free_shared(history, len) };
        SANCOV_HISTORY.with(|c| c.set(std::ptr::null_mut()));
    }

    let pool = SANCOV_POOL.with(|c| c.get());
    if !pool.is_null() {
        let slots = SANCOV_POOL_SLOTS.with(|c| c.get());
        if slots > 0 {
            unsafe { crate::shared_mem::free_shared(pool, slots * len) };
        }
        SANCOV_POOL.with(|c| c.set(std::ptr::null_mut()));
        SANCOV_POOL_SLOTS.with(|c| c.set(0));
    }
}

/// Zero the transfer buffer before forking a child.
///
/// No-op when sancov is unavailable or transfer buffer is null.
pub fn clear_transfer_buffer() {
    if !sancov_is_available() {
        return;
    }
    let transfer = SANCOV_TRANSFER.with(|c| c.get());
    if transfer.is_null() {
        return;
    }
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    unsafe {
        std::ptr::write_bytes(transfer, 0, len);
    }
}

// ---------------------------------------------------------------------------
// Child operations
// ---------------------------------------------------------------------------

/// Copy BSS counters to the shared transfer buffer.
///
/// Call in the child process before `_exit()` so the parent can
/// inspect coverage. No-op when sancov is unavailable.
pub fn copy_counters_to_shared() {
    if !sancov_is_available() {
        return;
    }
    let transfer = SANCOV_TRANSFER.with(|c| c.get());
    if transfer.is_null() {
        return;
    }
    let src = COUNTERS_PTR.load(Ordering::Relaxed);
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    unsafe {
        std::ptr::copy_nonoverlapping(src, transfer, len);
    }
}

/// Zero BSS counters after fork.
///
/// Call in the child process immediately after `fork()` so the child's
/// counters start from zero. No-op when sancov is unavailable.
pub fn reset_bss_counters() {
    if !sancov_is_available() {
        return;
    }
    let ptr = COUNTERS_PTR.load(Ordering::Relaxed);
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    unsafe {
        std::ptr::write_bytes(ptr, 0, len);
    }
}

// ---------------------------------------------------------------------------
// Parallel pool
// ---------------------------------------------------------------------------

/// Get or initialize the sancov pool for parallel exploration.
///
/// Returns the pool base pointer. Reuses the existing pool if it has
/// enough slots; otherwise frees and reallocates.
/// Returns null if sancov is unavailable or allocation fails.
pub fn get_or_init_sancov_pool(slot_count: usize) -> *mut u8 {
    if !sancov_is_available() {
        return std::ptr::null_mut();
    }
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    if len == 0 {
        return std::ptr::null_mut();
    }

    let existing = SANCOV_POOL.with(|c| c.get());
    let existing_slots = SANCOV_POOL_SLOTS.with(|c| c.get());

    if !existing.is_null() && existing_slots >= slot_count {
        return existing;
    }

    // Free old pool if too small
    if !existing.is_null() {
        unsafe {
            crate::shared_mem::free_shared(existing, existing_slots * len);
        }
        SANCOV_POOL.with(|c| c.set(std::ptr::null_mut()));
        SANCOV_POOL_SLOTS.with(|c| c.set(0));
    }

    match crate::shared_mem::alloc_shared(slot_count * len) {
        Ok(ptr) => {
            SANCOV_POOL.with(|c| c.set(ptr));
            SANCOV_POOL_SLOTS.with(|c| c.set(slot_count));
            ptr
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Return a pointer to slot `idx` within the sancov pool.
///
/// # Safety
///
/// Caller must ensure `idx < slot_count` and `pool_base` was returned
/// by [`get_or_init_sancov_pool`].
pub unsafe fn sancov_pool_slot(pool_base: *mut u8, idx: usize) -> *mut u8 {
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    // Safety: pool_base is valid for slot_count * len bytes, idx < slot_count
    unsafe { pool_base.add(idx * len) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bucketing_table() {
        assert_eq!(COUNT_CLASS_LOOKUP[0], 0);
        assert_eq!(COUNT_CLASS_LOOKUP[1], 1);
        assert_eq!(COUNT_CLASS_LOOKUP[2], 2);
        assert_eq!(COUNT_CLASS_LOOKUP[3], 4);
        for i in 4..=7 {
            assert_eq!(COUNT_CLASS_LOOKUP[i], 8, "bucket mismatch at {i}");
        }
        for i in 8..=15 {
            assert_eq!(COUNT_CLASS_LOOKUP[i], 16, "bucket mismatch at {i}");
        }
        for i in 16..=31 {
            assert_eq!(COUNT_CLASS_LOOKUP[i], 32, "bucket mismatch at {i}");
        }
        for i in 32..=127 {
            assert_eq!(COUNT_CLASS_LOOKUP[i], 64, "bucket mismatch at {i}");
        }
        for i in 128..=255 {
            assert_eq!(COUNT_CLASS_LOOKUP[i], 128, "bucket mismatch at {i}");
        }
    }

    #[test]
    fn test_novelty_detection_basic() {
        // First observation should be novel
        let mut buffer = [0u8; 8];
        let mut history = [0u8; 8];
        buffer[0] = 1; // edge 0 hit once

        let novel = has_new_coverage_inner(buffer.as_mut_ptr(), history.as_mut_ptr(), 8);
        assert!(novel);
        // History should now have the bucketed value
        assert_eq!(history[0], 1);
    }

    #[test]
    fn test_known_coverage_skipped() {
        let mut buffer = [0u8; 8];
        let mut history = [0u8; 8];

        // First pass: establish coverage
        buffer[0] = 1;
        let novel = has_new_coverage_inner(buffer.as_mut_ptr(), history.as_mut_ptr(), 8);
        assert!(novel);

        // Second pass: same coverage → not novel
        buffer[0] = 1; // re-set since bucketing was applied in-place
        let novel = has_new_coverage_inner(buffer.as_mut_ptr(), history.as_mut_ptr(), 8);
        assert!(!novel);
    }

    #[test]
    fn test_higher_bucket_is_novel() {
        let mut buffer = [0u8; 4];
        let mut history = [0u8; 4];

        // Hit once → bucket 1
        buffer[0] = 1;
        let novel = has_new_coverage_inner(buffer.as_mut_ptr(), history.as_mut_ptr(), 4);
        assert!(novel);
        assert_eq!(history[0], 1);

        // Hit 5 times → bucket 8 (higher than 1) → novel
        buffer[0] = 5;
        let novel = has_new_coverage_inner(buffer.as_mut_ptr(), history.as_mut_ptr(), 4);
        assert!(novel);
        assert_eq!(history[0], 8);

        // Hit 3 times → bucket 4 (lower than 8) → not novel
        buffer[0] = 3;
        let novel = has_new_coverage_inner(buffer.as_mut_ptr(), history.as_mut_ptr(), 4);
        assert!(!novel);
        assert_eq!(history[0], 8); // unchanged
    }

    #[test]
    fn test_zeros_skipped() {
        let mut buffer = [0u8; 8];
        let mut history = [0u8; 8];

        // All zeros → no novelty
        let novel = has_new_coverage_inner(buffer.as_mut_ptr(), history.as_mut_ptr(), 8);
        assert!(!novel);
    }

    #[test]
    fn test_unavailable_noop() {
        // COUNTERS_PTR is null in test builds (no LLVM instrumentation)
        assert!(!sancov_is_available());
        assert_eq!(sancov_edge_count(), 0);
        assert_eq!(sancov_edges_covered(), 0);
        assert!(!has_new_sancov_coverage());
        assert!(!has_new_sancov_coverage_from(std::ptr::null_mut()));

        // These should all be safe no-ops
        copy_counters_to_shared();
        reset_bss_counters();
        clear_transfer_buffer();
        cleanup_sancov_shared();

        let pool = get_or_init_sancov_pool(4);
        assert!(pool.is_null());
    }

    #[test]
    fn test_init_cleanup_lifecycle() {
        // When sancov is unavailable, init/cleanup are no-ops
        init_sancov_shared().expect("init should succeed as no-op");
        let transfer = SANCOV_TRANSFER.with(|c| c.get());
        assert!(transfer.is_null(), "no buffers allocated without sancov");
        cleanup_sancov_shared();
    }

    #[test]
    fn test_classify_counts_in_place() {
        let mut buf = [0u8, 1, 2, 3, 5, 10, 20, 50, 200];
        classify_counts(buf.as_mut_ptr(), buf.len());
        assert_eq!(buf, [0, 1, 2, 4, 8, 16, 32, 64, 128]);
    }
}
