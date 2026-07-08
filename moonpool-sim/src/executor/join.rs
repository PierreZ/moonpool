//! Join handles and yield primitives for the deterministic executor.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_task::FallibleTask;
use moonpool_core::JoinError;
use parking_lot::Mutex;

use super::TaskMeta;

/// A task's raw output: the value, or the payload of the panic that killed it
/// (captured by the `catch_unwind` wrapper installed at spawn).
pub(crate) type TaskResult<T> = Result<T, Box<dyn Any + Send>>;

/// Owned handle to a spawned task, mirroring `tokio::task::JoinHandle`
/// semantics.
///
/// - `.await` resolves to `Ok(output)`, or `Err(JoinError::Cancelled)` after
///   [`abort`](Self::abort), or `Err(JoinError::Panicked)` if the task
///   panicked (siblings and the executor are unaffected).
/// - Dropping the handle **detaches** the task: it keeps running to
///   completion. Only [`abort`](Self::abort) or dropping the whole
///   [`Executor`](super::Executor) cancels it.
/// - [`is_finished`](Self::is_finished) is a non-blocking completion probe.
#[derive(Debug)]
pub struct JoinHandle<T> {
    /// `None` after `abort()` consumed the inner task (async-task cancels a
    /// task when its `Task`/`FallibleTask` handle is dropped). The `Mutex`
    /// (uncontended, single-threaded) exists so `abort(&self)` can take the
    /// inner value while the handle satisfies the `Sync` bound that
    /// `TaskProvider::JoinHandle` requires.
    inner: Mutex<Option<FallibleTask<TaskResult<T>, TaskMeta>>>,
}

impl<T> JoinHandle<T> {
    pub(crate) fn new(task: FallibleTask<TaskResult<T>, TaskMeta>) -> Self {
        Self {
            inner: Mutex::new(Some(task)),
        }
    }

    /// Report whether the task has finished running (completed, panicked, or
    /// was aborted). Non-blocking; mirrors `tokio::task::JoinHandle::is_finished`.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        match self.inner.lock().as_ref() {
            Some(task) => task.is_finished(),
            None => true,
        }
    }

    /// Abort the task: it will never be polled again and its future is
    /// dropped. Awaiting the handle afterwards yields
    /// `Err(JoinError::Cancelled)`.
    pub fn abort(&self) {
        // Dropping the FallibleTask cancels the task (async-task semantics).
        drop(self.inner.lock().take());
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.lock();
        match inner.as_mut() {
            // Aborted through this handle.
            None => Poll::Ready(Err(JoinError::Cancelled)),
            Some(task) => match Pin::new(task).poll(cx) {
                Poll::Pending => Poll::Pending,
                // Cancelled elsewhere (executor drop) before completing.
                Poll::Ready(None) => Poll::Ready(Err(JoinError::Cancelled)),
                Poll::Ready(Some(Ok(output))) => Poll::Ready(Ok(output)),
                Poll::Ready(Some(Err(_panic_payload))) => Poll::Ready(Err(JoinError::Panicked)),
            },
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // Detach-on-drop (tokio parity): the task keeps running.
        if let Some(task) = self.inner.lock().take() {
            task.detach();
        }
    }
}

/// Future returned by [`yield_now`] and [`until_stalled`]: pends once,
/// waking its own waker, then completes on the next poll.
#[derive(Debug)]
pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// Yield control from inside a task.
///
/// The task is rescheduled into the ready queue and resumes at a
/// seeded-random position among the runnable tasks (there is no "back of
/// the queue" under randomized scheduling).
#[must_use = "futures do nothing unless awaited"]
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

/// Driver-only: resume after every currently-runnable task has been polled
/// to `Pending` or completion.
///
/// Awaited by the orchestrator's step loops between `sim.step()` calls.
/// Mechanically identical to [`yield_now`], but because
/// [`Executor::block_on`](super::Executor::block_on) only re-polls the
/// driver after a full `run_until_stalled` drain, awaiting this from the
/// driver guarantees the "all runnable tasks ran" contract that the old
/// tokio-FIFO `yield_now` dance only approximated.
///
/// # Panics
///
/// Debug builds panic when called from inside a task: a task awaiting
/// "drain until nothing is runnable" would deadlock against itself. Tasks
/// yield with [`yield_now`].
#[must_use = "futures do nothing unless awaited"]
pub fn until_stalled() -> YieldNow {
    debug_assert!(
        !super::in_task(),
        "until_stalled() is driver-only; tasks must use executor::yield_now()"
    );
    YieldNow { yielded: false }
}
