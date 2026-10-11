//! Portable, Antithesis-style assertion accounting for the moonpool
//! framework, with zero dependencies.
//!
//! This crate counts. It keeps a fixed-size table of assertion sites and a
//! table of per-value buckets, and it tells an exploration backend when
//! something globally new happens. It has no dependencies and compiles on
//! `wasm32-unknown-unknown`, macOS and Linux. The fifteen `assert_*!` macros
//! that tests use live in `moonpool-sim`, which records here and turns the
//! counts into a report.
//!
//! ## What it counts
//!
//! | Accounting | Backing function | `moonpool-sim` macros |
//! |------------|------------------|-----------------------|
//! | Boolean | [`assertion_bool`] | `assert_always!`, `assert_always_or_unreachable!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!` |
//! | Numeric, with a best-value watermark | [`assertion_numeric`] | `assert_always_greater_than!` and the seven other numeric comparisons |
//! | Compound, with a frontier | [`assertion_sometimes_all`] | `assert_sometimes_all!` |
//! | Per-value buckets, with a quality watermark | [`assertion_sometimes_each`] | `assert_sometimes_each!` |
//!
//! An **always** site that fails is a bug. A **sometimes** or **reachable**
//! site that never passes is a coverage gap. `moonpool-sim` reports them as
//! `assertion_violations` and `coverage_violations`, and its
//! `UntilCoverageStable` iteration control stops when the sometimes sites
//! have fired and coverage stops growing.
//!
//! ## Probes in shipped code
//!
//! Two macros in this crate are for production code. [`reachable!`] marks a
//! path that a simulation should walk (`FoundationDB`'s `CODE_PROBE`).
//! [`sometimes!`] marks an outcome some run should reach. Outside a
//! simulation no table is installed, and each call costs one pointer read.
//!
//! ```rust
//! fn on_lease_lost() {
//!     moonpool_assertions::reachable!("lease lost while serving a read");
//! }
//!
//! // Inert: no table is installed outside a simulation.
//! on_lease_lost();
//! assert!(moonpool_assertions::assertion_read_all().is_empty());
//! ```
//!
//! A site's identity is the 64-bit FNV-1a hash of its message ([`msg_hash`]).
//! A probe and a simulation's `assert_reachable!` with the same text share
//! one slot. **Never reword a message**: a new text is a new site.
//!
//! ## Driving the tables directly
//!
//! ```rust
//! use moonpool_assertions::{AssertKind, assertion_bool, assertion_read_all, clear, init};
//!
//! init(); // heap-allocated tables, one process
//! moonpool_assertions::sometimes!(true, "replica caught up");
//! assertion_bool(AssertKind::Always, true, 2 + 2 == 4, "arithmetic holds");
//!
//! let slots = assertion_read_all();
//! assert_eq!(slots.len(), 2);
//! assert!(slots.iter().all(|slot| slot.fail_count == 0));
//! clear();
//! ```
//!
//! ## Budgets
//!
//! The tables have a fixed size: [`MAX_ASSERTION_SLOTS`] (2048) sites and
//! [`MAX_EACH_BUCKETS`] (256) `sometimes_each` buckets of at most six identity
//! keys each. An evaluation that finds a table full is counted
//! ([`assertion_dropped_allocations`], [`each_bucket_dropped_allocations`]).
//! A site reached with a different kind, or a message that collides with
//! another, sets its `conflicted` flag. `moonpool-sim` reports both as
//! always-violations, so a lost evaluation is never silent.
//!
//! ## Regions and exploration
//!
//! The counts live in two memory regions. By default [`init`] allocates them
//! on the heap: one process, as on wasm or in a plain test run.
//! `moonpool-explorer` instead allocates `MAP_SHARED` memory and hands it to
//! [`install_region`], so forked worker processes add to one shared table. It
//! also installs [`DiscoveryHooks`]. Every first pass, watermark improvement,
//! frontier advance and new bucket then reaches its discovery journal exactly
//! once across all processes, guarded by an atomic latch. With no hook
//! installed the accounting is pure.
//!
//! See [`slots`] and [`buckets`] for the tables, [`region`] for the storage
//! model and [`hooks`] for the coupling to an exploration backend.

#![deny(missing_docs)]

pub mod buckets;
pub mod hooks;
pub mod region;
pub mod slots;
mod table;

pub use buckets::{
    EACH_BUCKET_MEM_SIZE, EachBucket, MAX_EACH_BUCKETS, assertion_sometimes_each,
    each_bucket_dropped_allocations, each_bucket_read_all, unpack_quality,
};
pub use hooks::{DiscoveryHooks, DiscoveryKind, clear_discovery_hooks, set_discovery_hooks};
pub use region::{assertion_table_ptr, clear, each_bucket_ptr, init, install_region, reset};
pub use slots::{
    ASSERTION_TABLE_MEM_SIZE, AssertCmp, AssertKind, AssertionSlot, AssertionSlotSnapshot,
    MAX_ASSERTION_SLOTS, assertion_bool, assertion_dropped_allocations, assertion_numeric,
    assertion_read_all, assertion_sometimes_all, msg_hash,
};

/// A probe: production code marks a path it walked (`FoundationDB`'s
/// `CODE_PROBE`).
///
/// Inert outside a simulation: with no table installed it costs one pointer
/// read. The slot is the hash of the message, so a probe and a simulation's
/// `assert_reachable!` with the same text share one slot. A site that no run
/// reaches is a coverage violation. Never reword a probe's message.
///
/// ```rust
/// moonpool_assertions::reachable!("snapshot installed from a peer");
/// ```
#[macro_export]
macro_rules! reachable {
    ($message:expr) => {
        $crate::assertion_bool($crate::AssertKind::Reachable, true, true, $message)
    };
}

/// An outcome production code expects some run to reach.
///
/// The same accounting as a simulation's `assert_sometimes!`: a site whose
/// condition is never true in any run is a coverage violation. Inert outside
/// a simulation.
///
/// ```rust
/// let retries = 3;
/// moonpool_assertions::sometimes!(retries > 2, "a request needed more than two retries");
/// ```
#[macro_export]
macro_rules! sometimes {
    ($condition:expr, $message:expr) => {
        $crate::assertion_bool($crate::AssertKind::Sometimes, true, $condition, $message)
    };
}
