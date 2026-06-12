//! Portable assertion accounting for the moonpool framework.
//!
//! This is the Antithesis-style assertion suite — boolean (always / sometimes /
//! reachable / unreachable), numeric guidance with watermarks, compound
//! sometimes-all with frontier tracking, and per-value `sometimes_each` buckets —
//! factored out of `moonpool-explorer` so it has **zero dependencies** and
//! compiles on `wasm32-unknown-unknown`, macOS, and Linux.
//!
//! # How it relates to exploration
//!
//! The counts live in a region of memory. By default [`init`] allocates that
//! region on the heap (single process — wasm, macOS, plain native test runs).
//! `moonpool-explorer` instead allocates a `MAP_SHARED` region and hands it to
//! [`install_region`], so the same accounting is visible across `fork`ed
//! children, and installs a [`DiscoveryHooks`] that turns "a new assertion state
//! was reached" into coverage marking and fork dispatch. With no hooks installed
//! the accounting is pure: assertions still record pass/fail/watermark/frontier,
//! which is all `UntilAllSometimesReached` and contract validation need.
//!
//! See [`hooks`] for the coupling surface and [`region`] for the storage model.

#![deny(missing_docs)]

pub mod buckets;
pub mod hooks;
pub mod region;
pub mod slots;

pub use buckets::{
    EACH_BUCKET_MEM_SIZE, EachBucket, MAX_EACH_BUCKETS, assertion_sometimes_each,
    each_bucket_read_all, unpack_quality,
};
pub use hooks::{DiscoveryHooks, clear_discovery_hooks, set_discovery_hooks};
pub use region::{
    assertion_table_ptr, clear, each_bucket_ptr, init, install_region, prepare_next_seed_reset,
    reset,
};
pub use slots::{
    ASSERTION_TABLE_MEM_SIZE, AssertCmp, AssertKind, AssertionSlot, AssertionSlotSnapshot,
    MAX_ASSERTION_SLOTS, assertion_bool, assertion_numeric, assertion_read_all,
    assertion_sometimes_all, msg_hash,
};
