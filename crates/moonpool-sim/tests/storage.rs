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
#[path = "storage/crash_model.rs"]
mod crash_model;
#[path = "storage/determinism.rs"]
mod determinism;
#[path = "storage/directory.rs"]
mod directory;
#[path = "storage/disk_failure.rs"]
mod disk_failure;
#[path = "storage/faults.rs"]
mod faults;
#[path = "storage/latency.rs"]
mod latency;
#[path = "storage/performance.rs"]
mod performance;
#[path = "storage/positioned.rs"]
mod positioned;
#[path = "storage/recovery.rs"]
mod recovery;
// Exercises TokioStorageProvider — only available with the tokio-providers feature.
#[cfg(feature = "tokio-providers")]
#[path = "storage/tokio_provider.rs"]
mod tokio_provider;
