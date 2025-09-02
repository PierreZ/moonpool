//! Task spawning abstraction for single-threaded simulation environments.

use async_trait::async_trait;
use std::future::Future;

pub mod tokio_provider;

/// Provider for spawning local tasks in single-threaded context.
///
/// This trait abstracts task spawning to enable both real tokio tasks
/// and simulation-controlled task scheduling while maintaining
/// deterministic execution in single-threaded environments.
#[async_trait(?Send)]
pub trait TaskProvider: Clone {
    /// Spawn a named task that runs on the current thread.
    ///
    /// The task will be executed using spawn_local to maintain
    /// single-threaded execution guarantees required for simulation.
    fn spawn_task<F>(&self, name: &str, future: F) -> tokio::task::JoinHandle<()>
    where
        F: Future<Output = ()> + 'static;
}
