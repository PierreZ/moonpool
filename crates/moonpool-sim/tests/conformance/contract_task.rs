//! Task provider contract: the invariants any drop-in for `tokio::spawn` must hold.

use moonpool_core::{Providers, TaskProvider};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Assert the [`TaskProvider`] spawns work, joins cleanly, and yields.
pub(crate) async fn task_contract<P: Providers>(p: &P) {
    // Spawned work runs and is observable after the handle joins.
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    let handle = p.task().spawn_task("conformance-task", async move {
        flag.store(true, Ordering::SeqCst);
    });
    handle.await.expect("spawned task should join cleanly");
    assert!(
        ran.load(Ordering::SeqCst),
        "spawned task's effect must be visible after join"
    );

    // yield_now() completes without hanging.
    p.task().yield_now().await;
}
