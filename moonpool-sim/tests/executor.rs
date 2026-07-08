//! Behavior suite for the moonpool deterministic executor.
//!
//! Covers the tokio-parity contracts the orchestrator relies on (join,
//! abort, detach-on-drop, panic isolation, kill-on-drop) and the
//! determinism properties that are the executor's reason to exist
//! (same seed same schedule, different seeds different schedules).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use moonpool_core::JoinError;
use moonpool_sim::executor::{Executor, JoinHandle, spawn, until_stalled, yield_now};
use parking_lot::Mutex;

#[test]
fn spawn_and_join_returns_the_output() {
    let mut executor = Executor::new(1);
    let value = executor.block_on(async {
        let handle = spawn("answer", async { 21 * 2 });
        handle.await.expect("task completed")
    });
    assert_eq!(value, 42);
}

#[test]
fn abort_cancels_and_reports_cancelled() {
    let mut executor = Executor::new(1);
    executor.block_on(async {
        let handle: JoinHandle<()> = spawn("parked", std::future::pending());
        assert!(!handle.is_finished());
        handle.abort();
        assert!(handle.is_finished());
        match handle.await {
            Err(JoinError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    });
}

#[test]
fn panicking_task_is_isolated() {
    let mut executor = Executor::new(1);
    let sibling_output = executor.block_on(async {
        let panicky: JoinHandle<()> = spawn("panicky", async { panic!("task boom") });
        let sibling = spawn("sibling", async { 7 });

        match panicky.await {
            Err(JoinError::Panicked) => {}
            other => panic!("expected Panicked, got {other:?}"),
        }
        // The sibling and the driver are unaffected by the panic.
        sibling.await.expect("sibling completed")
    });
    assert_eq!(sibling_output, 7);
}

#[test]
fn dropping_the_handle_detaches_the_task() {
    let mut executor = Executor::new(1);
    let ran = Arc::new(AtomicBool::new(false));
    let ran_clone = Arc::clone(&ran);
    executor.block_on(async move {
        let handle = spawn("detached", async move {
            ran_clone.store(true, Ordering::Relaxed);
        });
        drop(handle);
        // One full drain later, the detached task has still run.
        until_stalled().await;
    });
    assert!(ran.load(Ordering::Relaxed), "detached task must still run");
}

#[test]
fn until_stalled_drains_every_runnable_task() {
    let mut executor = Executor::new(1);
    let log: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    executor.block_on(async {
        for i in 0..8u64 {
            let log = Arc::clone(&log);
            drop(spawn(&format!("task-{i}"), async move {
                // Two poll rounds each: yield once, then record.
                yield_now().await;
                log.lock().push(i);
            }));
        }
        assert!(log.lock().is_empty(), "driver runs before any task");
        until_stalled().await;
        assert_eq!(log.lock().len(), 8, "all tasks polled to completion");
    });
}

/// Run 16 immediately-ready tasks and record their completion order.
fn completion_order(seed: u64) -> Vec<u64> {
    let mut executor = Executor::new(seed);
    let log: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    executor.block_on(async {
        for i in 0..16u64 {
            let log = Arc::clone(&log);
            drop(spawn(&format!("task-{i}"), async move {
                log.lock().push(i);
            }));
        }
        until_stalled().await;
    });
    let order = log.lock().clone();
    drop(executor);
    order
}

#[test]
fn same_seed_replays_the_same_schedule() {
    for seed in 0..4 {
        assert_eq!(
            completion_order(seed),
            completion_order(seed),
            "seed {seed} must replay identically"
        );
    }
}

#[test]
fn different_seeds_explore_different_schedules() {
    let orders: Vec<Vec<u64>> = (0..8).map(completion_order).collect();
    let distinct = orders
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert!(
        distinct > 1,
        "8 seeds should produce more than one task interleaving, got {orders:?}"
    );
    // And none of them is plain FIFO spawn order for ALL seeds (that would
    // mean the scheduler ignores the seed entirely).
    let fifo: Vec<u64> = (0..16).collect();
    assert!(
        orders.iter().any(|o| *o != fifo),
        "at least one seed must deviate from FIFO order"
    );
}

#[test]
fn dropping_the_executor_cancels_parked_tasks() {
    /// Sets a flag when the future holding it is dropped.
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicBool::new(false));

    let mut executor = Executor::new(1);
    let flag = DropFlag(Arc::clone(&dropped));
    let completed_clone = Arc::clone(&completed);
    executor.block_on(async move {
        drop(spawn("parked-forever", async move {
            let _flag = flag;
            std::future::pending::<()>().await;
            completed_clone.store(true, Ordering::Relaxed);
        }));
        // Let the task run up to its await point, then leave it parked.
        until_stalled().await;
    });

    assert!(!dropped.load(Ordering::Relaxed), "task is parked, not dead");
    drop(executor);
    assert!(
        dropped.load(Ordering::Relaxed),
        "executor drop must cancel the parked task and drop its future"
    );
    assert!(!completed.load(Ordering::Relaxed), "task must not complete");
}

#[test]
fn yield_now_requeues_the_task() {
    let mut executor = Executor::new(3);
    let value = executor.block_on(async {
        let counter = spawn("yielder", async {
            let mut n = 0;
            for _ in 0..5 {
                yield_now().await;
                n += 1;
            }
            n
        });
        counter.await.expect("yielder completed")
    });
    assert_eq!(value, 5);
}

#[test]
#[should_panic(expected = "deterministic executor stalled (seed 9)")]
fn genuine_deadlock_panics_with_the_seed() {
    let mut executor = Executor::new(9);
    executor.block_on(async {
        // Awaits a wake that can never arrive: no task, no timer, nothing.
        std::future::pending::<()>().await;
    });
}

#[test]
#[should_panic(expected = "executor::spawn called outside Executor::block_on")]
fn spawn_outside_block_on_panics() {
    drop(spawn("orphan", async {}));
}
