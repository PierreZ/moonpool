//! LLVM `SanitizerCoverage` (`inline-8bit-counters`) for code edge coverage.
//!
//! This module hooks into LLVM's `SanitizerCoverage` instrumentation to track
//! which code edges the simulation actually executes. Moonpool uses the
//! cumulative edge count for reporting and `UntilCoverageStable` plateau
//! detection. Frontier expansion itself is assertion-guided because only an
//! assertion discovery carries the RNG call-count anchor needed by a replay
//! recipe.
//!
//! All public functions are no-ops when sancov instrumentation is not
//! present (i.e., `COUNTERS_PTR` is null).
//!
//! # Coverage and exploration guidance
//!
//! Moonpool keeps two complementary progress signals:
//!
//! ```text
//! Signal                  What it tracks                    What consumes it
//! ──────────────────────  ────────────────────────────────  ─────────────────────
//! Assertion discoveries   Semantic states and watermarks    Frontier controller
//! Sancov edge coverage    Executed branches and conditions  Reports and plateau
//! ```
//!
//! A timeline can add code coverage without creating a frontier job. Add
//! semantic `assert_sometimes!`, `assert_sometimes_each!`, or numeric guidance
//! assertions at states from which the explorer should continue.
//!
//! # How LLVM inline-8bit-counters work
//!
//! When you compile with the right flags, LLVM inserts a `counter[edge_id]++`
//! instruction at every control-flow edge in the program. The counters live
//! in a BSS array (zero-initialized, process-global):
//!
//! ```text
//! rustc + LLVM passes
//!     │
//!     ▼
//! BSS counter array (one u8 per code edge)
//!     │
//!     ▼  __sanitizer_cov_8bit_counters_init(start, stop)
//!     │  called during static constructors, before main()
//!     │
//!     ▼
//! COUNTERS_PTR + COUNTERS_LEN captured in global atomics
//! ```
//!
//! Multiple compilation units each call the init callback with their own
//! array bounds. We merge them via `min(start)` / `max(stop)` so a single
//! contiguous range covers all TUs. See [`__sanitizer_cov_8bit_counters_init`].
//!
//! # Selective instrumentation with `SANCOV_CRATES`
//!
//! You don't want to instrument *everything* — the simulation runtime,
//! exploration engine, and chaos framework have thousands of edges that
//! are irrelevant to the system under test. Instrumenting only the
//! application crate(s) keeps the edge count small and meaningful.
//!
//! The build pipeline:
//!
//! ```text
//! flake.nix
//!   └── RUSTC_WRAPPER="$PWD/scripts/sancov-rustc.sh"
//!
//! sancov-rustc.sh intercepts every rustc invocation:
//!   1. Reads --crate-name from args
//!   2. Checks if crate is in SANCOV_CRATES (comma-separated whitelist)
//!   3. If yes → adds LLVM flags:
//!        -Cpasses=sancov-module
//!        -Cllvm-args=-sanitizer-coverage-level=3
//!        -Cllvm-args=-sanitizer-coverage-inline-8bit-counters
//!        -Ccodegen-units=1
//!   4. If no  → pass-through (no instrumentation)
//!   5. Build scripts and proc-macros are never instrumented
//! ```
//!
//! When `SANCOV_CRATES` is unset or empty, the wrapper is a pure
//! pass-through and all functions in this module are no-ops.
//!
//! The `xtask` runner (`cargo xtask sim`) sets `SANCOV_CRATES` per binary
//! and builds into `target/sancov` to avoid cache conflicts with normal
//! (non-instrumented) builds.
//!
//! # The shared memory data flow
//!
//! The BSS counters are process-local — a forked child increments its own
//! copy, but the parent can't see them. Shared memory bridges the gap:
//!
//! ```text
//! CHILD process                              PARENT process
//! ─────────────                              ──────────────
//! BSS counters increment
//! during simulation
//!     │
//!     ▼ copy_counters_to_shared()
//! TRANSFER buffer ──── MAP_SHARED ────────── classify_counts()
//! (or pool slot)                             bucketed values
//!                                                │
//!                                                ▼ has_new_coverage_inner()
//!                                            HISTORY map ── global max per edge
//!                                                │
//!                                                ▼
//!                                            cumulative reporting
//! ```
//!
//! Three shared memory regions:
//!
//! - **Transfer buffer** (`SANCOV_TRANSFER`): child writes raw counters
//!   via `copy_counters_to_shared` before `_exit()`. In sequential mode,
//!   one buffer is reused. In parallel mode, each concurrent child gets
//!   its own pool slot instead.
//!
//! - **History map** (`SANCOV_HISTORY`): global maximum of bucketed values
//!   per edge. It stays cumulative across seeds within one explorer run.
//!
//! - **Pool** (`SANCOV_POOL`): worker mode allocates
//!   `slot_count × edge_count` bytes. Each concurrent child writes to its
//!   own slot. Parent reads the slot after `waitpid()`. Allocated lazily
//!   by `init_sancov_pool`.
//!
//! # AFL-style bucketing
//!
//! Raw counter values are noisy — an edge hit 5 times vs 7 times is the
//! same execution pattern, but hit 1 time vs 5 times is meaningfully
//! different. The `COUNT_CLASS_LOOKUP` table maps raw counts to coarser
//! buckets, following AFL's proven approach:
//!
//! ```text
//! Raw count    Bucket    Meaning
//! ─────────    ──────    ─────────────────────
//!   0            0       not hit
//!   1            1       hit once
//!   2            2       hit twice
//!   3            4       hit a few times
//!   4–7          8       hit several times
//!   8–15        16       hit many times
//!  16–31        32       hit frequently
//!  32–127       64       hit very frequently
//! 128–255      128       hit extremely often
//! ```
//!
//! This means: an edge going from 5 hits to 7 hits (both bucket 8) is
//! not novel. But going from 1 hit to 5 hits (bucket 1 → bucket 8) *is*
//! novel — the code exercised that edge in a meaningfully different way.
//!
//! Bucketing is applied in-place by `classify_counts` before comparison.
//!
//! # Novelty detection
//!
//! `has_new_coverage_inner` is the core novelty check:
//!
//! 1. Apply AFL bucketing to the buffer in-place
//! 2. For each edge: if `bucketed > history[i]`, update history and mark novel
//! 3. Does **not** early-return — must update all history entries in one pass
//!    (otherwise a novel edge in position 100 would cause edges 101+ to
//!    be skipped, leaving stale history values)
//! 4. Zero entries (unvisited edges) are skipped
//!
//! The public API has two entry points:
//! - `has_new_sancov_coverage`: reads from the transfer buffer (sequential)
//! - `has_new_pool_coverage`: reads from a specific pool slot (parallel)
//!
//! # Integration with workers
//!
//! The frontier controller uses this module at four points:
//!
//! ```text
//! begin seed/job          reset_bss_counters() so the run captures its edges
//!
//! worker exit             copy_counters_to_shared() so
//!                        the parent can read the child's coverage.
//!
//! in-process completion   has_new_sancov_coverage() merges the transfer buffer
//!
//! worker reap             has_new_pool_coverage(slot) merges that worker
//!                        slot into cumulative history
//! ```
//!
//! # Why binary targets? (sancov requires `main()`)
//!
//! LLVM calls [`__sanitizer_cov_8bit_counters_init`] during static
//! constructors, before `main()`. In `cargo test`, the test harness is
//! `main()` — the counter array is initialized once for the harness, not
//! for each `#[test]` function. Worse, the BSS counters are process-global
//! state that accumulates across test functions, making per-test
//! measurement impossible. And `fork()` in a test harness interacts badly
//! with the harness's own process management.
//!
//! Solution: each simulation runs as a standalone `[[bin]]` target
//! managed by `cargo xtask sim`. The `xtask` sets `SANCOV_CRATES` per
//! binary and uses `--target-dir target/sancov` to separate instrumented
//! from non-instrumented builds.
//!
//! # Reporting
//!
//! Coverage stats flow through the reporting pipeline:
//!
//! - [`sancov_edge_count`] and [`sancov_edges_covered`] provide the raw numbers
//! - `moonpool-sim` carries them into its `ExplorationReport`
//! - The terminal display shows a "Code Cov" progress bar
//! - Percentage = `edges_covered / edges_total`
//!
//! # Running code coverage
//!
//! ```bash
//! # Run all simulations with sancov:
//! cargo xtask sim run-all
//!
//! # Run a specific simulation:
//! cargo xtask sim run maze
//!
//! # List available binaries:
//! cargo xtask sim list
//!
//! # Instrument specific crates manually:
//! SANCOV_CRATES=moonpool_sim_examples cargo run \
//!     --bin sim-maze-explore --target-dir target/sancov
//! ```
//!
//! # Lifecycle summary
//!
//! ```text
//! init_sancov_shared()          allocate transfer + history in MAP_SHARED
//!   │
//!   ├── per-child:
//!   │     reset_bss_counters()        zero BSS array after fork
//!   │     ... simulation runs ...     BSS counters increment
//!   │     copy_counters_to_shared()   copy BSS → transfer/pool slot
//!   │     exit_child()                _exit()
//!   │
//!   └── per-reap:
//!         has_new_sancov_coverage()   bucket + compare against history
//! ```

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::shared_mem::SharedMemory;

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
    /// Redirected to a worker's pool slot after `fork`.
    static SANCOV_TRANSFER: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };

    /// MAP_SHARED global max map (history of highest bucketed values).
    static SANCOV_HISTORY: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };

    /// Owners of the transfer and history pointers above.
    static SANCOV_REGIONS: RefCell<Option<(SharedMemory, SharedMemory)>> =
        const { RefCell::new(None) };

    /// MAP_SHARED pool for parallel mode (one slot per concurrent child) and
    /// its slot count. Initialized once for the bounded worker pool.
    static SANCOV_POOL: RefCell<Option<(SharedMemory, usize)>> = const { RefCell::new(None) };

    /// Cumulative "ever non-zero" mask over the BSS counter array (one byte per
    /// edge), for the fork-free live coverage reader. Lets the reader survive
    /// 8-bit counter wraparound: once an edge has been seen non-zero it stays
    /// counted, so the result is monotonic across seeds.
    static SANCOV_SEEN: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// The BSS counter array and its length, or `None` when sancov is unavailable.
fn counters() -> Option<(*mut u8, usize)> {
    let ptr = COUNTERS_PTR.load(Ordering::Relaxed);
    (!ptr.is_null()).then(|| (ptr, COUNTERS_LEN.load(Ordering::Relaxed)))
}

/// The history map, or `None` when sancov is unavailable or uninitialized.
fn history() -> Option<*mut u8> {
    let history = SANCOV_HISTORY.with(Cell::get);
    (sancov_is_available() && !history.is_null()).then_some(history)
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
    let new_len = unsafe { stop.offset_from(start) }.cast_unsigned();

    let prev_ptr = COUNTERS_PTR.load(Ordering::Relaxed);
    let prev =
        (!prev_ptr.is_null()).then(|| (prev_ptr as usize, COUNTERS_LEN.load(Ordering::Relaxed)));
    let (merged_start, merged_len) = merge_counter_range(prev, (start as usize, new_len));
    // Re-derive the pointer from `start` or `prev_ptr` to keep provenance.
    let merged_ptr = if merged_start == start as usize {
        start
    } else {
        prev_ptr
    };
    COUNTERS_PTR.store(merged_ptr, Ordering::Relaxed);
    COUNTERS_LEN.store(merged_len, Ordering::Relaxed);
}

/// Merge a newly registered counter range `(start, len)` into the range
/// tracked so far, returning the covering `(min_start, max_stop - min_start)`.
///
/// The previous range's end is computed from its *own* start before the start
/// moves, so registration order never changes the result.
fn merge_counter_range(prev: Option<(usize, usize)>, new: (usize, usize)) -> (usize, usize) {
    let (new_start, new_len) = new;
    let new_stop = new_start + new_len;
    match prev {
        None => (new_start, new_len),
        Some((prev_start, prev_len)) => {
            let prev_stop = prev_start + prev_len;
            let start = prev_start.min(new_start);
            let stop = prev_stop.max(new_stop);
            (start, stop - start)
        }
    }
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
    let mut i = 1;
    while i < 256 {
        table[i] = match i {
            1 => 1,
            2 => 2,
            3 => 4,
            4..=7 => 8,
            8..=15 => 16,
            16..=31 => 32,
            32..=127 => 64,
            _ => 128,
        };
        i += 1;
    }
    table
};

/// Apply AFL bucketing to a buffer of edge counts in-place.
fn classify_counts(buffer: &mut [u8]) {
    for count in buffer {
        *count = COUNT_CLASS_LOOKUP[usize::from(*count)];
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
fn has_new_coverage_inner(buffer: &mut [u8], history: &mut [u8]) -> bool {
    classify_counts(buffer);

    let mut found_new = false;
    for (&bucketed, prev) in buffer.iter().zip(history.iter_mut()) {
        if bucketed != 0 && bucketed > *prev {
            *prev = bucketed;
            found_new = true;
        }
    }
    found_new
}

/// Classify the `COUNTERS_LEN`-byte buffer at `buffer` and merge it into the
/// history map. Returns `false` when sancov or the history is unavailable.
fn merge_into_history(buffer: *mut u8) -> bool {
    let Some(history) = history() else {
        return false;
    };
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    // Safety: `buffer` (the transfer buffer or a pool slot) and `history` are
    // distinct live shared mappings of at least `len` bytes each.
    unsafe {
        has_new_coverage_inner(
            std::slice::from_raw_parts_mut(buffer, len),
            std::slice::from_raw_parts_mut(history, len),
        )
    }
}

/// Check for novel sancov coverage in the transfer buffer (sequential path).
///
/// Returns `false` when sancov is unavailable.
pub(crate) fn has_new_sancov_coverage() -> bool {
    let transfer = SANCOV_TRANSFER.with(Cell::get);
    !transfer.is_null() && merge_into_history(transfer)
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
    let Some(history) = history() else {
        return 0;
    };
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    // Safety: history was allocated with at least `len` bytes
    let history = unsafe { std::slice::from_raw_parts(history, len) };
    history.iter().filter(|&&bucket| bucket != 0).count()
}

/// Cumulative count of edges ever observed non-zero in the live BSS array.
///
/// Unlike [`sancov_edges_covered`], this reads the LLVM-instrumented counters
/// directly in the current process — no `SANCOV_HISTORY`, no fork. Each call
/// folds the current counters into a persistent internal "seen" mask
/// and returns the size of the union. Folding (rather than counting raw
/// non-zero bytes) is essential: the 8-bit counters wrap at 256 and are not
/// reset between seeds in the sequential path, so a hot edge whose cumulative
/// count momentarily lands on a multiple of 256 would otherwise drop out of a
/// raw count. The union is monotonic across seeds, so it can plateau.
///
/// Returns 0 when sancov is unavailable.
#[must_use]
pub fn sancov_edges_covered_live() -> usize {
    let Some((ptr, len)) = counters() else {
        return 0;
    };
    // Safety: the LLVM ctor registered `len` bytes starting at `ptr`.
    let live = unsafe { std::slice::from_raw_parts(ptr, len) };
    SANCOV_SEEN.with_borrow_mut(|seen| {
        if seen.len() != len {
            seen.clear();
            seen.resize(len, 0);
        }
        let mut count = 0usize;
        for (slot, &counter) in seen.iter_mut().zip(live) {
            if counter != 0 {
                *slot = 1;
            }
            if *slot != 0 {
                count += 1;
            }
        }
        count
    })
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
pub(crate) fn init_sancov_shared() -> Result<(), std::io::Error> {
    let Some((_, len)) = counters() else {
        return Ok(());
    };
    if len == 0 || SANCOV_REGIONS.with(|regions| regions.borrow().is_some()) {
        return Ok(());
    }

    let transfer = SharedMemory::new(len)?;
    let history = SharedMemory::new(len)?;

    SANCOV_TRANSFER.with(|c| c.set(transfer.as_ptr()));
    SANCOV_HISTORY.with(|c| c.set(history.as_ptr()));
    SANCOV_REGIONS.with(|regions| *regions.borrow_mut() = Some((transfer, history)));

    Ok(())
}

// ---------------------------------------------------------------------------
// Child operations
// ---------------------------------------------------------------------------

/// Copy BSS counters to the shared transfer buffer.
///
/// Call in the child process before `_exit()` so the parent can
/// inspect coverage. No-op when sancov is unavailable.
pub(crate) fn copy_counters_to_shared() {
    let Some((src, len)) = counters() else {
        return;
    };
    let transfer = SANCOV_TRANSFER.with(Cell::get);
    if transfer.is_null() {
        return;
    }
    // Safety: src points to the LLVM-generated BSS counter array (set by
    // __sanitizer_cov_8bit_counters_init), transfer points to a live shared mapping.
    // Both are valid for len bytes. The regions do not overlap (BSS vs mmap).
    unsafe {
        std::ptr::copy_nonoverlapping(src, transfer, len);
    }
}

/// Zero BSS counters after fork.
///
/// Call in the child process immediately after `fork()` so the child's
/// counters start from zero. No-op when sancov is unavailable.
pub(crate) fn reset_bss_counters() {
    let Some((ptr, len)) = counters() else {
        return;
    };
    // Safety: ptr points to the LLVM-generated BSS counter array (set by
    // __sanitizer_cov_8bit_counters_init). It is valid for len bytes and writable
    // (BSS is read-write). write_bytes zeroes exactly len bytes.
    unsafe {
        std::ptr::write_bytes(ptr, 0, len);
    }
}

// ---------------------------------------------------------------------------
// Parallel pool
// ---------------------------------------------------------------------------

/// Allocate the per-worker sancov pool (`slot_count × edge_count` bytes).
///
/// Reuses the existing pool if it has enough slots; otherwise frees and
/// reallocates. No-op when sancov is unavailable or allocation fails.
pub(crate) fn init_sancov_pool(slot_count: usize) {
    let Some((_, len)) = counters() else {
        return;
    };
    if len == 0 {
        return;
    }
    SANCOV_POOL.with_borrow_mut(|pool| {
        if pool.as_ref().is_some_and(|&(_, slots)| slots >= slot_count) {
            return;
        }
        // Free the old pool (if too small) before allocating its replacement.
        *pool = None;
        *pool = SharedMemory::array(slot_count, len)
            .ok()
            .map(|memory| (memory, slot_count));
    });
}

/// Pointer to pool slot `idx`, or `None` when sancov is unavailable, the
/// pool was not allocated, or `idx` is out of range.
fn pool_slot_ptr(idx: usize) -> Option<*mut u8> {
    SANCOV_POOL.with_borrow(|pool| {
        let (memory, slots) = pool.as_ref()?;
        if idx >= *slots {
            return None;
        }
        let len = COUNTERS_LEN.load(Ordering::Relaxed);
        // Safety: the pool covers slots * len bytes and idx < slots.
        Some(unsafe { memory.as_ptr().add(idx * len) })
    })
}

/// Zero one pool slot before handing it to a worker.
///
/// No-op when sancov is unavailable or the pool was not allocated.
pub(crate) fn clear_pool_slot(idx: usize) {
    let Some(slot_ptr) = pool_slot_ptr(idx) else {
        return;
    };
    let len = COUNTERS_LEN.load(Ordering::Relaxed);
    // Safety: the slot is valid for len bytes.
    unsafe {
        std::ptr::write_bytes(slot_ptr, 0, len);
    }
}

/// In a worker: point the transfer buffer at this worker's pool slot, so
/// `copy_counters_to_shared` lands in memory the controller can read back.
///
/// No-op when sancov is unavailable or the pool was not allocated.
pub(crate) fn redirect_transfer_to_pool_slot(idx: usize) {
    if let Some(slot_ptr) = pool_slot_ptr(idx) {
        SANCOV_TRANSFER.with(|c| c.set(slot_ptr));
    }
}

/// Classify one pool slot's counters and merge them into the history map.
///
/// Returns `true` when the slot contributed new coverage. `false` when
/// sancov is unavailable or the pool was not allocated.
pub(crate) fn has_new_pool_coverage(idx: usize) -> bool {
    pool_slot_ptr(idx).is_some_and(merge_into_history)
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
        for (i, &val) in COUNT_CLASS_LOOKUP.iter().enumerate().skip(4).take(4) {
            assert_eq!(val, 8, "bucket mismatch at {i}");
        }
        for (i, &val) in COUNT_CLASS_LOOKUP.iter().enumerate().skip(8).take(8) {
            assert_eq!(val, 16, "bucket mismatch at {i}");
        }
        for (i, &val) in COUNT_CLASS_LOOKUP.iter().enumerate().skip(16).take(16) {
            assert_eq!(val, 32, "bucket mismatch at {i}");
        }
        for (i, &val) in COUNT_CLASS_LOOKUP.iter().enumerate().skip(32).take(96) {
            assert_eq!(val, 64, "bucket mismatch at {i}");
        }
        for (i, &val) in COUNT_CLASS_LOOKUP.iter().enumerate().skip(128) {
            assert_eq!(val, 128, "bucket mismatch at {i}");
        }
    }

    #[test]
    fn test_novelty_detection_basic() {
        // First observation should be novel
        let mut buffer = [0u8; 8];
        let mut history = [0u8; 8];
        buffer[0] = 1; // edge 0 hit once

        let novel = has_new_coverage_inner(&mut buffer, &mut history);
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
        let novel = has_new_coverage_inner(&mut buffer, &mut history);
        assert!(novel);

        // Second pass: same coverage → not novel
        buffer[0] = 1; // re-set since bucketing was applied in-place
        let novel = has_new_coverage_inner(&mut buffer, &mut history);
        assert!(!novel);
    }

    #[test]
    fn test_higher_bucket_is_novel() {
        let mut buffer = [0u8; 4];
        let mut history = [0u8; 4];

        // Hit once → bucket 1
        buffer[0] = 1;
        let novel = has_new_coverage_inner(&mut buffer, &mut history);
        assert!(novel);
        assert_eq!(history[0], 1);

        // Hit 5 times → bucket 8 (higher than 1) → novel
        buffer[0] = 5;
        let novel = has_new_coverage_inner(&mut buffer, &mut history);
        assert!(novel);
        assert_eq!(history[0], 8);

        // Hit 3 times → bucket 4 (lower than 8) → not novel
        buffer[0] = 3;
        let novel = has_new_coverage_inner(&mut buffer, &mut history);
        assert!(!novel);
        assert_eq!(history[0], 8); // unchanged
    }

    #[test]
    fn test_zeros_skipped() {
        let mut buffer = [0u8; 8];
        let mut history = [0u8; 8];

        // All zeros → no novelty
        let novel = has_new_coverage_inner(&mut buffer, &mut history);
        assert!(!novel);
    }

    #[test]
    fn test_unavailable_noop() {
        // COUNTERS_PTR is null in test builds (no LLVM instrumentation)
        assert!(!sancov_is_available());
        assert_eq!(sancov_edge_count(), 0);
        assert_eq!(sancov_edges_covered(), 0);
        assert_eq!(sancov_edges_covered_live(), 0);
        assert!(!has_new_sancov_coverage());
        assert!(!has_new_pool_coverage(0));

        // These should all be safe no-ops
        copy_counters_to_shared();
        reset_bss_counters();

        init_sancov_pool(4);
        assert!(pool_slot_ptr(0).is_none());
    }

    #[test]
    fn test_init_lifecycle() {
        // When sancov is unavailable, init is a no-op
        init_sancov_shared().expect("init should succeed as no-op");
        let transfer = SANCOV_TRANSFER.with(std::cell::Cell::get);
        assert!(transfer.is_null(), "no buffers allocated without sancov");
    }

    /// Fold `ranges` in order through [`merge_counter_range`].
    fn merge_all(ranges: &[(usize, usize)]) -> (usize, usize) {
        ranges
            .iter()
            .fold(None, |acc, &r| Some(merge_counter_range(acc, r)))
            .expect("at least one range")
    }

    #[test]
    fn test_counter_range_merge_is_order_independent() {
        // Disjoint: [0x1000, 0x1100) and [0x0800, 0x0900).
        let disjoint = [(0x1000, 0x100), (0x0800, 0x100)];
        assert_eq!(merge_all(&disjoint), (0x0800, 0x900));
        assert_eq!(merge_all(&[disjoint[1], disjoint[0]]), (0x0800, 0x900));

        // Overlapping: [0x1000, 0x1200) and [0x1100, 0x1300).
        let overlap = [(0x1000, 0x200), (0x1100, 0x200)];
        assert_eq!(merge_all(&overlap), (0x1000, 0x300));
        assert_eq!(merge_all(&[overlap[1], overlap[0]]), (0x1000, 0x300));

        // Nested: an inner range never shrinks the span.
        let nested = [(0x1000, 0x400), (0x1100, 0x10)];
        assert_eq!(merge_all(&nested), (0x1000, 0x400));
        assert_eq!(merge_all(&[nested[1], nested[0]]), (0x1000, 0x400));

        // Three ranges, every permutation agrees.
        let three = [(0x2000, 0x10), (0x0800, 0x20), (0x1000, 0x5)];
        let expected = (0x0800, 0x2010 - 0x0800);
        for perm in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let ordered = perm.map(|i| three[i]);
            assert_eq!(merge_all(&ordered), expected);
        }
    }

    #[test]
    fn test_classify_counts_in_place() {
        let mut buf = [0u8, 1, 2, 3, 5, 10, 20, 50, 200];
        classify_counts(&mut buf);
        assert_eq!(buf, [0, 1, 2, 4, 8, 16, 32, 64, 128]);
    }
}
