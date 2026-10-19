//! Tasks on the compio runtime: [`CompioTaskProvider`] and its
//! [`CompioJoinHandle`].

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use moonpool_core::{Detach, JoinError, LocalTaskProvider, TaskProvider};

/// Task provider over `compio::runtime::spawn`.
///
/// Every task runs on the runtime thread that spawned it: compio is
/// thread-per-core and never moves a task. That is why this provider also
/// implements [`LocalTaskProvider`]: a future holding `Rc<RefCell<…>>` is
/// spawned with [`spawn_local`](LocalTaskProvider::spawn_local).
///
/// Spawning needs a running compio runtime on the calling thread (it panics
/// otherwise, as `compio::runtime::spawn` does).
#[derive(Debug, Clone, Copy, Default)]
pub struct CompioTaskProvider;

impl CompioTaskProvider {
    /// Create a compio task provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// The handle [`CompioTaskProvider`] returns.
///
/// Resolves with `Ok(())` when the task completes, or with
/// [`JoinError::Cancelled`] / [`JoinError::Panicked`]. Dropping it detaches
/// the task, as [`Detach`] promises: compio's own handle cancels on drop, so
/// this wrapper calls compio's `detach` instead.
#[derive(Debug)]
pub struct CompioJoinHandle {
    /// `None` once the task's outcome has been returned, or after `detach`.
    inner: Option<compio::runtime::JoinHandle<()>>,
}

impl Future for CompioJoinHandle {
    type Output = Result<(), JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(handle) = self.inner.as_mut() else {
            // Polled again after completion: the task already reported.
            return Poll::Ready(Err(JoinError::Cancelled));
        };
        let outcome = std::task::ready!(Pin::new(handle).poll(cx));
        // A finished handle has nothing left to cancel or detach.
        self.inner = None;
        Poll::Ready(outcome.map_err(|error| match error {
            compio::runtime::JoinError::Cancelled => JoinError::Cancelled,
            compio::runtime::JoinError::Panicked(_) => JoinError::Panicked,
        }))
    }
}

impl Detach for CompioJoinHandle {
    fn detach(mut self) {
        if let Some(handle) = self.inner.take() {
            handle.detach();
        }
    }
}

impl Drop for CompioJoinHandle {
    fn drop(&mut self) {
        // moonpool's contract: dropping a handle never cancels the task.
        if let Some(handle) = self.inner.take() {
            handle.detach();
        }
    }
}

/// Wrap `future` with the start and end trace events every moonpool task
/// provider emits.
fn traced<F>(name: &str, future: F) -> impl Future<Output = ()> + use<F>
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

impl TaskProvider for CompioTaskProvider {
    type JoinHandle = CompioJoinHandle;

    #[tracing::instrument(skip_all, fields(task = name))]
    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        CompioJoinHandle {
            inner: Some(compio::runtime::spawn(traced(name, future))),
        }
    }

    fn yield_now(&self) -> impl Future<Output = ()> + Send {
        YieldNow { yielded: false }
    }
}

impl LocalTaskProvider for CompioTaskProvider {
    #[tracing::instrument(skip_all, fields(task = name))]
    fn spawn_local<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + 'static,
    {
        CompioJoinHandle {
            inner: Some(compio::runtime::spawn(traced(name, future))),
        }
    }
}

/// compio has no `yield_now`: return `Pending` once, waking this task so the
/// executor runs it again after the other ready tasks.
#[derive(Debug)]
struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            return Poll::Ready(());
        }
        self.yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
