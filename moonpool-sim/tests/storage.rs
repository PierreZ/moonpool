//! Storage simulation tests module.
//!
//! Contains tests for the storage simulation subsystem.

#[path = "storage/basic.rs"]
mod basic;
#[path = "storage/determinism.rs"]
mod determinism;
#[path = "storage/faults.rs"]
mod faults;
#[path = "storage/latency.rs"]
mod latency;
