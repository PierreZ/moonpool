//! Frontier-based exploration for deterministic simulation testing.
//!
//! This crate turns deterministic simulation runs into *accumulated
//! exploration knowledge*. Timelines that reach globally-new interesting
//! states (reported by the assertion macros) are remembered as replayable
//! recipes; a central controller schedules bounded continuations from those
//! states; timelines that produce nothing new simply die. The logical
//! exploration space can be huge while the physical footprint stays fixed:
//! one controller process plus at most `workers` short-lived worker
//! processes.
//!
//! # Glossary
//!
//! ```text
//! Term          What it means
//! ────────────  ──────────────────────────────────────────────────────────
//! Seed          A u64 that completely determines a simulation's randomness.
//!
//! Timeline      One complete simulation run: a root seed plus a recipe.
//!
//! Recipe        Replay breakpoints: a list of (rng_call_count, seed)
//!               segments. "Run with the root seed; after N RNG calls,
//!               reseed to S and keep going." Replaying a recipe reproduces
//!               its timeline exactly, scheduler decisions included.
//!
//! Discovery     A globally-new interesting state: the first pass of an
//!               assert_sometimes!/assert_reachable!, a new
//!               assert_sometimes_each! bucket, a partial truth combination,
//!               or a numeric/frontier/quality improvement. Guarded by atomic
//!               state in the shared assertion region, so each recorded
//!               transition is observed once across all timelines and seeds.
//!
//! Journal       The list of discoveries one run made, each stamped with the
//!               RNG call count where it happened.
//!
//! Frontier      FIFO queue of jobs (recipes) waiting to be explored.
//!
//! Exemplar      A retained recipe + anchor that reaches one semantic state.
//!               A few are kept per state (same bucket, different futures).
//!
//! Worker        A forked process that executes exactly one timeline and
//!               exits. Workers never fork and never make decisions.
//! ```
//!
//! # The big idea
//!
//! A normal simulation picks one seed and runs one timeline. Bugs hide in
//! rare states, so independent seeds keep rediscovering shallow states. The
//! explorer instead capitalizes on progress:
//!
//! ```text
//! root seed ──────────▶ run ── journal: {floor 2 reached @ rng call 812}
//!                                  │ productive → expand once
//!                     +────────────+────────────+
//!                     ▼            ▼            ▼
//!               replay to 812  replay to 812  replay to 812   (workers)
//!               + new seed A   + new seed B   + new seed C
//!                     │            │            │
//!               nothing new     floor 3!     nothing new
//!               (branch dies)  @ call 1490  (branch dies)
//!                                  │ expand once
//!                                 ...
//! ```
//!
//! Deep states are reached by *extending recipes*, not by keeping processes
//! alive: reaching floor 8 costs a recipe with ~8 segments, not a tree of
//! live processes. When the frontier drains, the controller schedules fresh
//! continuations from the least-visited known state (preferring monotonic
//! progress states and deeper recipes), so the budget keeps pushing the
//! frontier instead of hammering the start.
//!
//! # Process model
//!
//! ```text
//!            controller (owns frontier, exemplars, stats)
//!            │ fork            │ fork             │ fork
//!            ▼                 ▼                  ▼
//!        worker 0          worker 1           worker 2      ≤ workers live
//!        run 1 timeline    run 1 timeline     run 1 timeline
//!        write journal     write journal      write journal
//!        _exit             _exit              _exit
//! ```
//!
//! Forks happen at a quiescent point between runs (never mid-simulation),
//! the controller is single-threaded, and only portable POSIX primitives are
//! used (`fork`, `waitpid`, `mmap MAP_SHARED`) — this works on macOS as well
//! as Linux. With `workers == 0` everything runs in-process: sequential,
//! fully deterministic, fork-free.
//!
//! # Who decides what
//!
//! - **Assertion macros** describe interesting behavior. They never fork and
//!   never encode exploration policy.
//! - **The shared assertion region** (moonpool-assertions) decides global
//!   novelty: one CAS latch per distinct discovery, race-free across worker
//!   processes.
//! - **The controller** ([`Explorer`]) owns the frontier, the exemplar
//!   store, all statistics, and the bug-recipe list. It is the single owner
//!   of every exploration decision.
//! - **The runner** (moonpool-sim) executes timelines: it installs the
//!   recipe's RNG breakpoints, runs the simulation, and reports failure.
//!
//! # Reproducing bugs
//!
//! A failing worker's job recipe is retained verbatim. Replaying it —
//! same root seed, same breakpoints — reproduces the failing timeline
//! bit-for-bit, including task scheduling: the executor draws its schedule
//! from the same counted stream the recipe replays.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

use std::cell::RefCell;

pub mod controller;
pub mod journal;
pub mod replay;
pub mod sancov;
mod shared_mem;
pub mod simulations;
pub mod worker;

pub use controller::{ExplorationConfig, ExplorationStats, ExploreJob, Explorer};
pub use journal::{DiscoveryEvent, set_rng_count_hook};
pub use replay::{ParseTimelineError, Recipe, format_timeline, parse_timeline};
pub use sancov::{
    sancov_edge_count, sancov_edges_covered, sancov_edges_covered_live, sancov_is_available,
};
pub use worker::explorer_is_child;

use shared_mem::SharedMemory;

thread_local! {
    /// Owners for the pointers installed into `moonpool-assertions`.
    static ASSERTION_REGIONS: RefCell<Option<(SharedMemory, SharedMemory)>> = const {
        RefCell::new(None)
    };
}

// The assertion + each-bucket accounting lives in the dependency-free,
// wasm-able `moonpool-assertions` crate. Re-export the surface callers reach
// through this crate.
pub use moonpool_assertions::{
    ASSERTION_TABLE_MEM_SIZE, AssertCmp, AssertKind, AssertionSlot, AssertionSlotSnapshot,
    DiscoveryKind, EACH_BUCKET_MEM_SIZE, EachBucket, MAX_ASSERTION_SLOTS, MAX_EACH_BUCKETS,
    assertion_bool, assertion_dropped_allocations, assertion_numeric, assertion_read_all,
    assertion_sometimes_all, assertion_sometimes_each, assertion_table_ptr, each_bucket_read_all,
    msg_hash, unpack_quality,
};

/// Initialize the assertion table and each-bucket accounting in `MAP_SHARED`
/// memory, so counts and discovery latches are visible across forked worker
/// processes. Idempotent (no-op if already initialized).
///
/// # Errors
///
/// Returns an error if shared memory allocation fails.
pub fn init_assertions() -> Result<(), std::io::Error> {
    if ASSERTION_REGIONS.with(|regions| regions.borrow().is_some()) {
        return Ok(());
    }

    let table = SharedMemory::new(ASSERTION_TABLE_MEM_SIZE)?;
    let buckets = SharedMemory::new(EACH_BUCKET_MEM_SIZE)?;
    // Safety: `SharedMemory` returns non-null, page-aligned, zeroed mappings of
    // exactly the required lengths. The thread-local owners remain live until
    // `cleanup_assertions` first clears the accounting pointers.
    unsafe { moonpool_assertions::install_region(table.as_ptr(), buckets.as_ptr()) };
    ASSERTION_REGIONS.with(|regions| *regions.borrow_mut() = Some((table, buckets)));
    Ok(())
}

/// Free the assertion table and each-bucket shared memory.
///
/// Nulls the pointers after freeing. No-op if not initialized.
pub fn cleanup_assertions() {
    moonpool_assertions::clear();
    ASSERTION_REGIONS.with(|regions| regions.borrow_mut().take());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_assertions_standalone() {
        // A plain assertion run may have installed heap regions first. The
        // explorer must replace those rather than mistaking them for shared
        // mappings that forked workers can observe.
        moonpool_assertions::init();
        assertion_bool(AssertKind::Sometimes, true, true, "heap-only");
        assert_eq!(assertion_read_all().len(), 1);

        init_assertions().expect("init_assertions failed");
        assert!(!assertion_table_ptr().is_null());
        assert!(assertion_read_all().is_empty());

        // Idempotent — second call should be no-op
        init_assertions().expect("init_assertions second call failed");

        cleanup_assertions();
        assert!(assertion_table_ptr().is_null());
    }

    #[test]
    fn test_assertion_bool_noop_when_inactive() {
        // Should not panic when assertion table is not initialized
        assertion_bool(AssertKind::Sometimes, true, true, "test_assertion");
    }
}
