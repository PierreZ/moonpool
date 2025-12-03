//! End-to-end simulation tests for Peer correctness.
//!
//! These tests use the Operation Alphabet Pattern with Invariant Tracking
//! to verify Peer behavior under chaos conditions.

#[path = "e2e/mod.rs"]
mod e2e_mod;

// Re-export common types for test modules
pub use e2e_mod::*;
