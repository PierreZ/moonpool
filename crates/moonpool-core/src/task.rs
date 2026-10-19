//! Tasks: [`TaskProvider`], with [`TokioTaskProvider`] for production, and
//! [`LocalTaskProvider`] for futures that are not `Send`.
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

/// Provider for spawning futures that are not `Send`.
///
/// Implemented by runtimes that never move a task between threads: the
/// simulator, compio, and tokio's local flavors ([`TokioLocalTaskProvider`]).
/// It is not implemented by the work-stealing [`TokioTaskProvider`], so code
/// bounded on this trait cannot be run there by mistake.
///
/// Thread-per-core code bounds on this trait (or on
/// [`LocalProviders`](crate::LocalProviders)) and keeps its state in
/// `Rc<RefCell<…>>` instead of `Arc<RwLock<…>>`. The futures the other
/// providers return (`sleep`, `connect`, `read_at`, ...) are `Send`, and a
/// `Send` future may be awaited inside a local task, so every provider stays
/// usable there. Use [`TimeProvider::timeout_local`](crate::TimeProvider::timeout_local)
/// to bound a future that is itself not `Send`.
///
/// ```rust
/// use std::{cell::RefCell, rc::Rc};
///
/// use moonpool_core::{Detach, LocalTaskProvider};
///
/// fn start_counter<T: LocalTaskProvider>(tasks: &T, hits: Rc<RefCell<u64>>) {
///     tasks
///         .spawn_local("counter", async move { *hits.borrow_mut() += 1 })
///         .detach();
/// }
/// ```
pub trait LocalTaskProvider: TaskProvider {
    /// Spawn a named task whose future may hold `Rc`, `RefCell` and other
    /// thread-bound state.
    ///
    /// The task runs on the thread that spawned it. Returns the same handle
    /// type as [`TaskProvider::spawn_task`]; the handle itself is `Send` and
    /// resolves with `Ok(())` on normal completion, or a [`JoinError`] if the
    /// task was cancelled or panicked.
    fn spawn_local<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + 'static;
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
        TokioJoinHandle(tokio::spawn(traced_task(name, future)))
    }

    async fn yield_now(&self) {
        tokio::task::yield_now().await;
    }
}

/// Wrap a spawned future in the start and completion trace events every
/// moonpool task provider emits (Tokio's here, compio's in `moonpool-compio`).
///
/// The returned future is `Send` exactly when `future` is, so the same
/// wrapper feeds both the `Send` spawn paths and the local ones. Not public
/// API: exported for the backends only.
pub fn traced_task<F>(name: &str, future: F) -> impl Future<Output = ()> + use<F>
where
    F: Future<Output = ()>,
{
    let task_name = name.to_string();
    async move {
        tracing::trace!("Task {} starting", task_name);
        future.await;
        tracing::trace!("Task {} completed", task_name);
    }
}

/// Tokio task provider for the local flavors, where tasks never leave the
/// thread that spawned them.
///
/// [`spawn_task`](TaskProvider::spawn_task) behaves exactly like
/// [`TokioTaskProvider`] (`tokio::spawn`, same trace events).
/// [`spawn_local`](LocalTaskProvider::spawn_local) calls
/// `tokio::task::spawn_local`, which tokio only accepts inside a
/// `tokio::task::LocalSet` (stable) or on a `tokio::runtime::LocalRuntime`
/// (`--cfg tokio_unstable`, `#[tokio::main(flavor = "local")]`). Anywhere
/// else tokio panics with its own message. Moonpool adds no cfg of its own:
/// pick the runtime, then hand this provider to the code.
///
/// ```rust
/// use std::{cell::RefCell, rc::Rc};
///
/// use moonpool_core::{JoinError, LocalTaskProvider, TokioLocalTaskProvider};
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<(), JoinError> {
/// let local = tokio::task::LocalSet::new();
/// local
///     .run_until(async {
///         let hits = Rc::new(RefCell::new(0_u32));
///         let counter = Rc::clone(&hits);
///         TokioLocalTaskProvider
///             .spawn_local("counter", async move { *counter.borrow_mut() += 1 })
///             .await?;
///         assert_eq!(*hits.borrow(), 1);
///         Ok(())
///     })
///     .await
/// # }
/// ```
#[cfg(feature = "tokio-task")]
#[derive(Clone, Debug)]
pub struct TokioLocalTaskProvider;

#[cfg(feature = "tokio-task")]
impl TaskProvider for TokioLocalTaskProvider {
    type JoinHandle = TokioJoinHandle;

    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        TokioJoinHandle(tokio::spawn(traced_task(name, future)))
    }

    async fn yield_now(&self) {
        tokio::task::yield_now().await;
    }
}

#[cfg(feature = "tokio-task")]
impl LocalTaskProvider for TokioLocalTaskProvider {
    fn spawn_local<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + 'static,
    {
        TokioJoinHandle(tokio::task::spawn_local(traced_task(name, future)))
    }
}

#[cfg(all(test, feature = "tokio-task"))]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::{LocalTaskProvider, TaskProvider, TokioJoinHandle, TokioLocalTaskProvider};

    fn assert_send<T: Send>() {}

    #[test]
    fn tokio_join_handle_is_send() {
        assert_send::<TokioJoinHandle>();
    }

    #[tokio::test]
    async fn spawn_local_runs_a_non_send_future() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let hits = Rc::new(RefCell::new(0_u32));
                let counter = Rc::clone(&hits);
                let handle = TokioLocalTaskProvider.spawn_local("counter", async move {
                    tokio::task::yield_now().await;
                    *counter.borrow_mut() += 1;
                });
                assert!(handle.await.is_ok());
                assert_eq!(*hits.borrow(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn spawn_task_still_spawns_send_futures() {
        let handle = TokioLocalTaskProvider.spawn_task("send", async {
            tokio::task::yield_now().await;
        });
        assert!(handle.await.is_ok());
    }
}
