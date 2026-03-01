//! LLVM SanitizerCoverage (`inline-8bit-counters`) for code edge coverage.
//!
//! This module hooks into LLVM's SanitizerCoverage instrumentation to track
//! which code edges the simulation actually executes. The explorer uses this
//! as a second coverage signal — alongside the assertion-path fork bitmap —
//! to decide whether a forked timeline discovered something new. Together,
//! the two signals make the adaptive loop significantly more precise at
//! distinguishing productive forks from barren ones.
//!
//! All public functions are no-ops when sancov instrumentation is not
//! present (i.e., `COUNTERS_PTR` is null).
//!
//! # Why two coverage systems?
//!
//! The explorer has two independent coverage tracking mechanisms:
//!
//! ```text
//! System                Where it lives         What it tracks
//! ────────────────────  ─────────────────────  ─────────────────────────────────
//! Fork bitmap           coverage.rs            Which assert_sometimes! /
//!   (8192 bits)                                assert_sometimes_each! fired.
//!                                              Hash-based, high collision rate.
//!
//! Sancov edge coverage  sancov.rs (this file)  Which branches/loops/conditions
//!   (one u8 per edge)                          in the Rust source were executed.
//!                                              LLVM-instrumented, no collisions.
//! ```
//!
//! The fork bitmap answers "did we trigger a new assertion path?" while
//! sancov answers "did we execute new code?". Two timelines can trigger
//! the exact same assertions but take radically different code paths through
//! the system under test — sancov catches that.
//!
//! Both signals feed into the adaptive loop's `batch_has_new` flag in
//! [`split_loop`](crate::split_loop). A timeline is considered productive
//! if it contributes new bits to **either** system:
//!
//! ```text
//! batch_has_new |= fork_bitmap.has_new_bits()    // assertion-level
//! batch_has_new |= has_new_sancov_coverage()      // code-edge-level
//! ```
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
//!                                            batch_has_new = true if any
//!                                            bucketed > history[i]
//! ```
//!
//! Three shared memory regions:
//!
//! - **Transfer buffer** ([`SANCOV_TRANSFER`]): child writes raw counters
//!   via [`copy_counters_to_shared`] before `_exit()`. In sequential mode,
//!   one buffer is reused. In parallel mode, each concurrent child gets
//!   its own pool slot instead.
//!
//! - **History map** (`SANCOV_HISTORY`): global maximum of bucketed values
//!   per edge. Never reset within a seed. Preserved across seeds by
//!   [`prepare_next_seed()`](crate::prepare_next_seed) (cumulative, like
//!   the explored map).
//!
//! - **Pool** ([`SANCOV_POOL`]): parallel mode allocates
//!   `slot_count × edge_count` bytes. Each concurrent child writes to its
//!   own slot. Parent reads the slot after `waitpid()`. Allocated lazily
//!   by [`get_or_init_sancov_pool`].
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
//! - [`has_new_sancov_coverage`]: reads from the transfer buffer (sequential)
//! - [`has_new_sancov_coverage_from`]: reads from a specific pool slot (parallel)
//!
//! Novelty feeds into `batch_has_new` in [`split_loop`](crate::split_loop)
//! alongside the fork bitmap's
//! [`has_new_bits()`](crate::coverage::ExploredMap::has_new_bits).
//!
//! # Integration with the fork loop
//!
//! This module hooks into [`split_loop`](crate::split_loop) at five points:
//!
//! ```text
//! setup_child()          After fork: reset_bss_counters() so child
//!                        captures only its OWN edges. Reset pool pointers
//!                        so nested splits allocate fresh pools.
//!
//! exit_child()           Before _exit(): copy_counters_to_shared() so
//!                        the parent can read the child's coverage.
//!
//! Sequential reap        After waitpid: has_new_sancov_coverage() checks
//!                        the transfer buffer for novelty.
//!
//! Parallel reap          After waitpid: has_new_sancov_coverage_from(slot)
//!                        checks the child's pool slot for novelty.
//!
//! batch_has_new          Both coverage signals are OR'd together:
//!                        batch_has_new |= sancov_novelty
//!                        A barren/productive decision uses BOTH signals.
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
//! - [`ExplorationStats`](crate::shared_stats::ExplorationStats) includes
//!   `sancov_edges_total` and `sancov_edges_covered`
//! - `ExplorationReport` carries them to the `SimulationReport`
//! - The terminal display shows a "Code Cov" progress bar alongside
//!   the "Exploration" (fork bitmap) progress bar
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
//!   ├── per-reap:
//!   │     has_new_sancov_coverage()   bucket + compare against history
//!   │
//!   ├── prepare_next_seed():
//!   │     clear_transfer_buffer()     zero transfer buffer
//!   │     reset_bss_counters()        zero BSS array
//!   │     (history preserved)         cumulative across seeds
//!   │
//!   └── cleanup_sancov_shared()      free transfer + history + pool
//! ```

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
