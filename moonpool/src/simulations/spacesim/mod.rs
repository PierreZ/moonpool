//! Galactic cargo hauling network simulation.
//!
//! Idempotent cargo operations with op_id dedup for crash-safe
//! at-least-once delivery. No reconciliation needed.

pub mod actors;
pub mod invariants;
pub mod model;
pub mod operations;
pub mod workloads;
