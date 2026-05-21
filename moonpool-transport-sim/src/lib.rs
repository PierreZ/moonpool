//! Hash-chain simulation workload for moonpool-transport.
//!
//! A hash-chained append-only log exercises the transport end-to-end:
//! per-response reference-model checks catch any wire-level mutation, and a
//! replay-based invariant catches reorders, drops, and duplicates of
//! server-side timeline events.

pub mod hash;
pub mod invariants;
pub mod process;
pub mod service;
pub mod workload;
