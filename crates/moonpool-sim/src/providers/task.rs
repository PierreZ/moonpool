//! Task provider backed by the moonpool deterministic executor.

use std::future::Future;

use moonpool_core::TaskProvider;
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
/// # Panics
///
/// `spawn_task` panics when used outside `Executor::block_on` (mirroring
/// `tokio::spawn` outside a runtime).
#[derive(Clone, Debug, Default)]
pub struct SimTaskProvider;

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
        crate::executor::spawn(name, future.instrument(tracing::Span::current()))
    }

    async fn yield_now(&self) {
        crate::executor::yield_now().await;
    }
}
