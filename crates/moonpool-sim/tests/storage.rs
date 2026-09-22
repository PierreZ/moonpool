//! Storage simulation tests module.
//!
//! Contains tests for the storage simulation subsystem.

use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use moonpool_sim::{SimStorageProvider, SimWorld};

#[path = "common/runtime.rs"]
mod runtime;
#[path = "common/storage.rs"]
mod support;

use runtime::local_runtime;
use support::{fast_sim, run_as, run_on, step_until_done, test_ip};

/// Run one storage scenario on an owned world, stepping it until the task
/// finishes.
async fn run_storage_test<F, Fut, T>(mut sim: SimWorld, f: F) -> T
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    run_on(&mut sim, f).await
}

/// Run a storage scenario as `ip` and return the simulation time elapsed.
async fn run_and_measure_time_on<F, Fut>(mut sim: SimWorld, ip: IpAddr, f: F) -> Duration
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: Future<Output = std::io::Result<()>> + Send + 'static,
{
    run_as(&mut sim, ip, f).await.expect("io error");
    sim.current_time()
}

/// Run a storage scenario on [`test_ip`] and return the simulation time
/// elapsed.
async fn run_and_measure_time<F, Fut>(sim: SimWorld, f: F) -> Duration
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: Future<Output = std::io::Result<()>> + Send + 'static,
{
    run_and_measure_time_on(sim, test_ip(), f).await
}

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
#[path = "storage/namespaces.rs"]
mod namespaces;
#[path = "storage/performance.rs"]
mod performance;
#[path = "storage/positioned.rs"]
mod positioned;
#[path = "storage/recovery.rs"]
mod recovery;
// Both exercise TokioStorageProvider — only available with the tokio-providers
// feature.
#[cfg(feature = "tokio-providers")]
#[path = "storage/parity.rs"]
mod parity;
#[cfg(feature = "tokio-providers")]
#[path = "storage/tokio_provider.rs"]
mod tokio_provider;
