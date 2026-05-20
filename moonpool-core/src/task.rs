//! Task spawning abstraction for single-threaded simulation environments.
//!
//! This module provides task provider abstractions for spawning local tasks
//! that work with both simulation and real Tokio execution.

use async_trait::async_trait;
use std::future::Future;

/// Error returned by [`TaskProvider::JoinHandle`] when a task did not complete
/// normally.
///
/// This is the runtime-agnostic error surfaced by the [`TaskProvider`] trait.
/// Implementations convert their runtime-specific join error into one of these
/// variants.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    /// The task was cancelled (for example, the runtime aborted it).
    #[error("task was cancelled")]
    Cancelled,
    /// The task panicked.
    #[error("task panicked")]
    Panicked,
}

/// Provider for spawning local tasks in single-threaded context.
///
/// This trait abstracts task spawning to enable both real tokio tasks
/// and simulation-controlled task scheduling while maintaining
/// deterministic execution in single-threaded environments.
#[async_trait(?Send)]
pub trait TaskProvider: Clone {
    /// Future returned by [`Self::spawn_task`].
    ///
    /// Resolves with `Ok(())` on normal completion, or a [`JoinError`] if the
    /// task was cancelled or panicked.
    type JoinHandle: Future<Output = Result<(), JoinError>> + 'static;

    /// Spawn a named task that runs on the current thread.
    ///
    /// The task will be executed using spawn_local to maintain
    /// single-threaded execution guarantees required for simulation.
    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
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
#[cfg(feature = "tokio-providers")]
#[derive(Clone, Debug)]
pub struct TokioTaskProvider;

/// JoinHandle produced by [`TokioTaskProvider`].
///
/// Wraps tokio's `JoinHandle<()>` and converts the runtime-specific
/// `tokio::task::JoinError` into the runtime-agnostic [`JoinError`] variants
/// when polled.
#[cfg(feature = "tokio-providers")]
#[derive(Debug)]
pub struct TokioJoinHandle(tokio::task::JoinHandle<()>);

#[cfg(feature = "tokio-providers")]
impl Future for TokioJoinHandle {
    type Output = Result<(), JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        match std::pin::Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) if e.is_cancelled() => Poll::Ready(Err(JoinError::Cancelled)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(JoinError::Panicked)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(feature = "tokio-providers")]
#[async_trait(?Send)]
impl TaskProvider for TokioTaskProvider {
    type JoinHandle = TokioJoinHandle;

    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + 'static,
    {
        let task_name = name.to_string();
        let inner = tokio::task::Builder::new()
            .name(name)
            .spawn_local(async move {
                tracing::trace!("Task {} starting", task_name);
                future.await;
                tracing::trace!("Task {} completed", task_name);
            })
            .expect("Failed to spawn task");
        TokioJoinHandle(inner)
    }

    async fn yield_now(&self) {
        tokio::task::yield_now().await;
    }
}
