//! Tokio-based task provider implementation.

use async_trait::async_trait;
use std::future::Future;

use super::TaskProvider;

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
}
