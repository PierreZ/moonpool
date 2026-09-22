//! Run storage scenarios as Tokio tasks against a simulated world, stepping
//! the world until each task finishes.
//!
//! Shared by the test binaries that include it with `#[path]`.

use std::future::Future;
use std::net::IpAddr;

use moonpool_sim::{SimStorageProvider, SimWorld, StorageConfiguration};

/// The IP every single-node storage test runs as.
pub fn test_ip() -> IpAddr {
    "127.0.0.1".parse().expect("valid IP")
}

/// A `SimWorld` with the fast, fault-free storage configuration.
pub fn fast_sim() -> SimWorld {
    let mut sim = SimWorld::new();
    sim.set_storage_config(StorageConfiguration::fast_local());
    sim
}

/// Step `sim` until `handle`'s task finishes, then return its output.
pub async fn step_until_done<T: Send + 'static>(
    sim: &mut SimWorld,
    handle: tokio::task::JoinHandle<T>,
) -> T {
    while !handle.is_finished() {
        while sim.pending_event_count() > 0 {
            sim.step();
        }
        tokio::task::yield_now().await;
    }
    handle.await.expect("task panicked")
}

/// Run one storage scenario as `ip`, stepping the world until it finishes.
pub async fn run_as<F, Fut, T>(sim: &mut SimWorld, ip: IpAddr, f: F) -> T
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let provider = sim.storage_provider(ip);
    let handle = tokio::spawn(f(provider));
    step_until_done(sim, handle).await
}

/// Run one storage scenario as [`test_ip`].
pub async fn run_on<F, Fut, T>(sim: &mut SimWorld, f: F) -> T
where
    F: FnOnce(SimStorageProvider) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    run_as(sim, test_ip(), f).await
}
