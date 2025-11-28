//! End-to-end simulation tests for Peer correctness.
//!
//! These tests use the Operation Alphabet Pattern with Invariant Tracking
//! to verify Peer behavior under chaos conditions.

#[path = "e2e/mod.rs"]
mod e2e_mod;

#[path = "e2e/invariants.rs"]
mod invariants;

#[path = "e2e/operations.rs"]
mod operations;

#[path = "e2e/workloads.rs"]
mod workloads;

// Re-export for test modules
use e2e_mod::*;

// Include tests
#[path = "e2e/tests.rs"]
mod tests;
