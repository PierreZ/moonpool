//! Tasks: [`TaskProvider`], with [`TokioTaskProvider`] for production, and
//! [`LocalTaskProvider`], with [`TokioLocalTaskProvider`], for tasks that stay
//! on their thread and need not be `Send`.
//!
//! Application code spawns through this trait, never through
//! `tokio::spawn`. In simulation the spawned task runs on moonpool's
//! deterministic single-threaded executor, which picks the next ready task
//! from the seed and charges the task to the process that spawned it. A
//! reboot of that process cancels its tasks.
//!
//! ```rust
//! use moonpool_core::{Detach, TaskProvider};
//!
//! fn start_heartbeat<T: TaskProvider>(tasks: &T) {
//!     // The name shows in traces; detach states that nobody joins it.
//!     tasks.spawn_task("heartbeat", async { /* loop { send; sleep } */ }).detach();
//! }
//! ```

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
/// Production uses [`TokioTaskProvider`]. Simulation uses
/// `moonpool_sim::SimTaskProvider`. The simulation runs on one OS thread, but
/// spawned futures are still `Send`, so application code can use
/// `Arc<RwLock<…>>`, `DashMap` and other `Send + Sync` types without
/// change.
pub trait TaskProvider: Clone + Send + Sync + 'static {
    /// Future returned by [`Self::spawn_task`].
    ///
    /// Resolves with `Ok(())` on normal completion, or a [`JoinError`] if the
    /// task was cancelled or panicked. Consume it with [`Detach::detach`] for
    /// explicit fire-and-forget.
    type JoinHandle: Future<Output = Result<(), JoinError>> + Detach + Send + 'static;

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

/// Local (non-`Send`) task provider: spawns futures that stay on the
/// spawning thread.
///
/// A thread-per-core runtime (compio, monoio, a Tokio `LocalSet`) runs every
/// task on the thread that spawned it, so the future need not be `Send` and
/// may hold `Rc`, `RefCell` and the runtime's own thread-bound handles.
///
/// ## Relationship to [`TaskProvider`]
///
/// Unlike the other local traits, there is **no blanket impl** in either
/// direction:
///
/// - a [`TaskProvider`] cannot spawn a `!Send` future (`tokio::spawn` moves
///   it to any worker thread), so it is not a `LocalTaskProvider`;
/// - a `LocalTaskProvider` pins tasks to one thread and may return `!Send`
///   handles, so it is not a [`TaskProvider`].
///
/// A backend that can do both implements both:
/// [`TokioLocalTaskProvider`] is the local one for Tokio, and
/// `moonpool_sim::SimTaskProvider` implements both traits, because the
/// simulator runs every task on one thread anyway.
///
/// The method is named [`spawn_local_task`](Self::spawn_local_task), not
/// `spawn_task`, so a type implementing both traits never makes a call site
/// ambiguous. [`yield_now`](Self::yield_now) shares its name with
/// [`TaskProvider::yield_now`]: with both traits in scope, call it through
/// the trait (`LocalTaskProvider::yield_now(&tasks)`).
pub trait LocalTaskProvider: Clone + 'static {
    /// Future returned by [`Self::spawn_local_task`].
    ///
    /// Resolves with `Ok(())` on normal completion, or a [`JoinError`] if the
    /// task was cancelled or panicked. Consume it with [`Detach::detach`] for
    /// explicit fire-and-forget. It need not be `Send`.
    type JoinHandle: Future<Output = Result<(), JoinError>> + Detach + 'static;

    /// Spawn a named task on the current thread. The future need not be
    /// `Send`.
    fn spawn_local_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + 'static;

    /// Yield control to allow other tasks on this thread to run. See
    /// [`TaskProvider::yield_now`].
    fn yield_now(&self) -> impl Future<Output = ()>;
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
        std::pin::Pin::new(&mut self.0).poll(cx).map_err(|e| {
            if e.is_cancelled() {
                JoinError::Cancelled
            } else {
                JoinError::Panicked
            }
        })
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

/// Tokio-based local task provider: spawns `!Send` futures with
/// `tokio::task::spawn_local`.
///
/// Spawned tasks run on the current thread. Like `spawn_local`, it **panics**
/// unless called inside a `tokio::task::LocalSet` (or a local runtime): a
/// plain multi-threaded or current-thread runtime is not enough. The task
/// name is emitted in `tracing::trace!` events around the future, as
/// [`TokioTaskProvider`] does.
///
/// ```rust
/// use std::rc::Rc;
///
/// use moonpool_core::{LocalTaskProvider, TokioLocalTaskProvider};
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let local = tokio::task::LocalSet::new();
/// local
///     .run_until(async {
///         let shared = Rc::new(41);
///         let handle = TokioLocalTaskProvider.spawn_local_task("rc", async move {
///             assert_eq!(*shared + 1, 42);
///         });
///         handle.await.expect("task completed");
///     })
///     .await;
/// # }
/// ```
#[cfg(feature = "tokio-task")]
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioLocalTaskProvider;

#[cfg(feature = "tokio-task")]
impl LocalTaskProvider for TokioLocalTaskProvider {
    type JoinHandle = TokioJoinHandle;

    fn spawn_local_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + 'static,
    {
        let task_name = name.to_string();
        let fut = async move {
            tracing::trace!("Task {} starting", task_name);
            future.await;
            tracing::trace!("Task {} completed", task_name);
        };
        TokioJoinHandle(tokio::task::spawn_local(fut))
    }

    async fn yield_now(&self) {
        tokio::task::yield_now().await;
    }
}
