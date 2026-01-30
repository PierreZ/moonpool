//! Storage simulation tests module.
//!
//! Contains tests for the storage simulation subsystem.

#[path = "storage/basic.rs"]
mod basic;
#[path = "storage/concurrent.rs"]
mod concurrent;
#[path = "storage/config.rs"]
mod config;
#[path = "storage/crash_api.rs"]
mod crash_api;
#[path = "storage/determinism.rs"]
mod determinism;
#[path = "storage/faults.rs"]
mod faults;
#[path = "storage/latency.rs"]
mod latency;
#[path = "storage/performance.rs"]
mod performance;
#[path = "storage/recovery.rs"]
mod recovery;
#[path = "storage/tokio_provider.rs"]
mod tokio_provider;
