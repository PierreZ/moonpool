//! Task spawning abstraction for single-threaded simulation environments.
//!
//! This module provides task provider abstractions for spawning local tasks
//! that work with both simulation and real Tokio execution.

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

/// Explicit fire-and-forget for spawned tasks.
///
/// Consuming a join handle with [`detach`](Self::detach) leaves the task
/// running to completion in the background; its completion can no longer be
/// observed through the handle. Dropping the handle has the same runtime
/// behavior (tokio parity: tasks are never cancelled implicitly), but
/// `detach()` states the intent at the call site — and satisfies the
/// `must_use` lint that a discarded generic [`TaskProvider::JoinHandle`]
/// (an `impl Future`) would otherwise trigger.
pub trait Detach {
    /// Detach the task behind this handle, leaving it running.
    fn detach(self);
}

/// Provider for spawning tasks.
///
/// This trait abstracts task spawning to enable both real tokio tasks
/// and simulation-controlled task scheduling. The simulation runtime
/// runs on a single OS thread, but the spawned futures are Send-bounded
/// so customer call graphs can use `Arc<RwLock<…>>`, `DashMap`, and other
/// `Send + Sync` primitives without contortion.
pub trait TaskProvider: Clone + Send + Sync + 'static {
    /// Future returned by [`Self::spawn_task`].
    ///
    /// Resolves with `Ok(())` on normal completion, or a [`JoinError`] if the
    /// task was cancelled or panicked. Consume it with [`Detach::detach`] for
    /// explicit fire-and-forget.
    type JoinHandle: Future<Output = Result<(), JoinError>> + Detach + Send + Sync + 'static;

    /// Spawn a named task.
    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static;

    /// Yield control to allow other tasks to run.
    ///
    /// This is equivalent to `tokio::task::yield_now()` but abstracted
    /// to enable simulation control and deterministic behavior.
    fn yield_now(&self) -> impl Future<Output = ()> + Send;
}

/// Tokio-based task provider.
///
/// This provider creates tasks via `tokio::spawn`. The task name is emitted
/// in `tracing::trace!` spans around the future rather than attached to the
/// tokio task itself.
#[cfg(feature = "tokio-task")]
#[derive(Clone, Debug)]
pub struct TokioTaskProvider;

/// `JoinHandle` produced by [`TokioTaskProvider`].
///
/// Wraps tokio's `JoinHandle<()>` and converts the runtime-specific
/// `tokio::task::JoinError` into the runtime-agnostic [`JoinError`] variants
/// when polled.
#[cfg(feature = "tokio-task")]
#[derive(Debug)]
pub struct TokioJoinHandle(tokio::task::JoinHandle<()>);

#[cfg(feature = "tokio-task")]
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

#[cfg(feature = "tokio-task")]
impl Detach for TokioJoinHandle {
    fn detach(self) {
        // tokio join handles already detach on drop; consuming self is all
        // that is needed.
    }
}

#[cfg(feature = "tokio-task")]
impl TaskProvider for TokioTaskProvider {
    type JoinHandle = TokioJoinHandle;

    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task_name = name.to_string();
        let fut = async move {
            tracing::trace!("Task {} starting", task_name);
            future.await;
            tracing::trace!("Task {} completed", task_name);
        };
        TokioJoinHandle(tokio::spawn(fut))
    }

    async fn yield_now(&self) {
        tokio::task::yield_now().await;
    }
}
