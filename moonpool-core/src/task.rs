//! Task spawning abstraction for single-threaded simulation environments.
//!
//! This module provides task provider abstractions for spawning local tasks
//! that work with both simulation and real Tokio execution.

use async_trait::async_trait;
use std::future::Future;

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

    /// Yield control to allow other tasks to run.
    ///
    /// This is equivalent to tokio::task::yield_now() but abstracted
    /// to enable simulation control and deterministic behavior.
    async fn yield_now(&self);
}

/// Tokio-based task provider using spawn_local for single-threaded execution.
///
/// This provider creates tasks that run on the current thread using tokio's
/// spawn_local mechanism, ensuring compatibility with simulation environments
/// that require deterministic single-threaded execution.
#[derive(Clone, Debug)]
pub struct TokioTaskProvider;

#[async_trait(?Send)]
impl TaskProvider for TokioTaskProvider {
    fn spawn_task<F>(&self, name: &str, future: F) -> tokio::task::JoinHandle<()>
    where
        F: Future<Output = ()> + 'static,
    {
        let task_name = name.to_string();
        let task_name_clone = task_name.clone();
        tokio::task::Builder::new()
            .name(&task_name)
            .spawn_local(async move {
                tracing::trace!("Task {} starting", task_name_clone);
                future.await;
                tracing::trace!("Task {} completed", task_name_clone);
            })
            .expect("Failed to spawn task")
    }

    async fn yield_now(&self) {
        tokio::task::yield_now().await;
    }
}
