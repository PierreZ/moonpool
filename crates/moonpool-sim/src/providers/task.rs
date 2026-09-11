//! Task provider backed by the moonpool deterministic executor.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use moonpool_core::TaskProvider;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};
use tracing::Instrument as _;

/// Task provider that spawns onto the [deterministic
/// executor](crate::executor) driving the current simulation iteration.
///
/// Where `TokioTaskProvider` binds tasks to the ambient tokio runtime, this
/// provider binds them to the executor installed by
/// [`Executor::block_on`](crate::executor::Executor::block_on): scheduling
/// order is a seeded-random, fully reproducible function of the iteration
/// seed.
///
/// # Attribution
///
/// A spawned task runs inside the span that was current when it was spawned,
/// as `tokio::spawn` does. That span is (or descends from) the `process` /
/// `workload` span the orchestrator wraps around each actor, which is how
/// the observability layer attributes an event to its actor: an event emitted
/// from a background request handler reaches the invariant timeline exactly
/// as one emitted from the actor's root future does, rather than being
/// dropped for lacking a source.
///
/// # Ownership
///
/// A process's provider carries the **scope** of the boot it belongs to
/// ([`SimProviders::with_task_scope`](crate::SimProviders::with_task_scope)):
/// every task spawned through it, and every task those spawn in turn, is
/// dropped when the process is killed. A crash therefore ends the whole
/// process — a detached worker does not survive it to run beside the
/// rebooted process's own, as it would if the child were merely handed to
/// the shared executor. A killed scope is checked *before* the child future
/// is polled, so no application work runs after the kill. Workload providers
/// carry no scope: a workload survives everything.
///
/// # Panics
///
/// `spawn_task` panics when used outside `Executor::block_on` (mirroring
/// `tokio::spawn` outside a runtime).
#[derive(Clone, Debug, Default)]
pub struct SimTaskProvider {
    scope: Option<CancellationToken>,
}

impl SimTaskProvider {
    /// A provider whose spawned tasks live no longer than `scope`.
    pub(crate) fn scoped(scope: CancellationToken) -> Self {
        Self { scope: Some(scope) }
    }
}

impl TaskProvider for SimTaskProvider {
    type JoinHandle = crate::executor::JoinHandle<()>;

    fn spawn_task<F>(&self, name: &str, future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // No lifecycle-trace wrapper (unlike TokioTaskProvider, where the
        // name would otherwise be lost): the executor stores the name in
        // TaskMeta and traces every poll of the task with it. The child does
        // inherit the spawner's span, so its events keep their actor.
        let future = future.instrument(tracing::Span::current());
        match &self.scope {
            Some(scope) => crate::executor::spawn(name, Scoped::new(scope.clone(), future)),
            None => crate::executor::spawn(name, future),
        }
    }

    async fn yield_now(&self) {
        crate::executor::yield_now().await;
    }
}

/// A task future bound to a process boot: completes, dropping the inner
/// future unpolled, as soon as the boot's scope is cancelled.
///
/// The cancellation is polled *first* on every poll. A kill wakes every
/// task waiting on the scope; when the executor next runs the child, it
/// finds the scope cancelled and drops the application future without
/// giving it another poll, which is the "no application work after the
/// kill" rule the process manager keeps for the root future by aborting it.
struct Scoped<F> {
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    inner: Pin<Box<F>>,
}

impl<F: Future<Output = ()>> Scoped<F> {
    fn new(scope: CancellationToken, inner: F) -> Self {
        Self {
            cancelled: Box::pin(scope.cancelled_owned()),
            inner: Box::pin(inner),
        }
    }
}

impl<F: Future<Output = ()>> Future for Scoped<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        self.inner.as_mut().poll(cx)
    }
}
