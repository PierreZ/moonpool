//! `CompioTaskProvider`: spawning, `!Send` tasks, detach-on-drop, yields.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use moonpool_compio::{CompioTaskProvider, CompioTimeProvider};
use moonpool_core::{Detach, JoinError, LocalTaskProvider, TaskProvider, TimeProvider};

#[compio::test]
async fn a_spawned_task_resolves_its_handle() {
    let tasks = CompioTaskProvider::new();
    assert!(tasks.spawn_task("noop", async {}).await.is_ok());
}

#[compio::test]
async fn a_panicking_task_reports_it() {
    let tasks = CompioTaskProvider::new();
    let outcome = tasks
        .spawn_task("boom", async { panic!("deliberate panic in a test task") })
        .await;
    assert!(matches!(outcome, Err(JoinError::Panicked)));
}

#[compio::test]
async fn spawn_local_runs_non_send_futures() {
    let tasks = CompioTaskProvider::new();
    let counter = Rc::new(RefCell::new(0_u32));
    let inside = Rc::clone(&counter);
    tasks
        .spawn_local("rc", async move {
            *inside.borrow_mut() += 1;
        })
        .await
        .expect("the local task finished");
    assert_eq!(*counter.borrow(), 1);
}

/// Wait on real time, up to two seconds, for `flag`.
async fn eventually(time: &CompioTimeProvider, flag: &AtomicBool) -> bool {
    for _ in 0..200 {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        time.sleep(Duration::from_millis(10)).await.expect("sleep");
    }
    flag.load(Ordering::SeqCst)
}

/// compio's own handle cancels its task on drop; moonpool's contract is
/// that dropping (or detaching) a handle leaves the task running.
#[compio::test]
async fn dropping_or_detaching_a_handle_does_not_cancel_the_task() {
    let tasks = CompioTaskProvider::new();
    let time = CompioTimeProvider::new();
    for detach in [false, true] {
        let finished = Arc::new(AtomicBool::new(false));
        let handle = tasks.spawn_task("background", {
            let finished = Arc::clone(&finished);
            let time = time.clone();
            async move {
                time.sleep(Duration::from_millis(20)).await.expect("sleep");
                finished.store(true, Ordering::SeqCst);
            }
        });
        if detach {
            handle.detach();
        } else {
            drop(handle);
        }
        assert!(
            eventually(&time, &finished).await,
            "the task was cancelled (detach: {detach})"
        );
    }
}

/// Two tasks that yield after each step interleave: the second one runs
/// before the first one is done.
#[compio::test]
async fn yield_now_lets_other_tasks_run() {
    let tasks = CompioTaskProvider::new();
    let log = Rc::new(RefCell::new(Vec::new()));
    let spawn = |name: char| {
        let log = Rc::clone(&log);
        let yielder = tasks;
        tasks.spawn_local("yielder", async move {
            for _ in 0..3 {
                log.borrow_mut().push(name);
                yielder.yield_now().await;
            }
        })
    };
    let (a, b) = (spawn('a'), spawn('b'));
    a.await.expect("a finished");
    b.await.expect("b finished");
    let log = log.borrow();
    let first_b = log.iter().position(|&c| c == 'b').expect("b ran");
    let last_a = log.iter().rposition(|&c| c == 'a').expect("a ran");
    assert!(first_b < last_a, "no interleaving: {log:?}");
}
