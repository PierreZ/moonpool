//! Task provider backed by the moonpool deterministic executor.

use std::future::Future;

use moonpool_core::TaskProvider;

/// Task provider that spawns onto the [deterministic
/// executor](crate::executor) driving the current simulation iteration.
///
/// Where `TokioTaskProvider` binds tasks to the ambient tokio runtime, this
/// provider binds them to the executor installed by
/// [`Executor::block_on`](crate::executor::Executor::block_on): scheduling
/// order is a seeded-random, fully reproducible function of the iteration
/// seed.
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
        let task_name = name.to_string();
        crate::executor::spawn(name, async move {
            tracing::trace!("Task {} starting", task_name);
            future.await;
            tracing::trace!("Task {} completed", task_name);
        })
    }

    async fn yield_now(&self) {
        crate::executor::yield_now().await;
    }
}
