//! `!Send` tasks through `LocalTaskProvider::spawn_local` on the simulator.
//!
//! A process or workload keeps its `Send` shell (`run` is `#[async_trait]`)
//! and moves its `Rc`/`RefCell` state into a local task. The local task
//! must behave like a `spawn_task` child in every way the runner relies on:
//! the same seeded queue (replays bit for bit), the same boot scope (a crash
//! drops it), the same panic reporting, and provider futures awaited inside
//! it work as usual.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::JoinError;
use moonpool_sim::{
    FaultContext, FaultInjector, LocalTaskProvider, Process, RandomProvider, SimContext,
    SimulationBuilder, SimulationError, SimulationResult, TaskProvider, TimeError, TimeProvider,
    TraceEvent, Workload, assert_always, assert_sometimes,
};

fn sleep_failed(error: &TimeError) -> SimulationError {
    SimulationError::InvalidState(format!("sleep failed: {error}"))
}

fn join_failed(error: &JoinError) -> SimulationError {
    SimulationError::InvalidState(format!("local task failed: {error}"))
}

/// Keeps the run alive long enough for the processes to play out.
struct PatientWorkload;

#[async_trait]
impl Workload for PatientWorkload {
    fn name(&self) -> &'static str {
        "patient"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..200 {
            ctx.time()
                .sleep(Duration::from_millis(5))
                .await
                .map_err(|e| sleep_failed(&e))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// a. A local task holds an Rc across provider awaits and finishes.
// ---------------------------------------------------------------------------

/// Counts in an `Rc<RefCell<u32>>` across sleeps inside a local task.
struct RcCounterProcess;

#[async_trait]
impl Process for RcCounterProcess {
    fn name(&self) -> &'static str {
        "rc_counter"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let key = format!("rc_counter:{}", ctx.my_ip());
        let time = ctx.time().clone();
        let state = ctx.state().clone();
        let published = key.clone();
        ctx.task()
            .spawn_local("body", async move {
                let counter = Rc::new(RefCell::new(0u32));
                for _ in 0..3 {
                    // The clone lives across the await: this future is !Send.
                    let held = Rc::clone(&counter);
                    if time.sleep(Duration::from_millis(10)).await.is_err() {
                        return;
                    }
                    *held.borrow_mut() += 1;
                }
                state.publish(&published, *counter.borrow());
            })
            .await
            .map_err(|e| join_failed(&e))?;
        assert_always!(
            ctx.state().get::<u32>(&key) == Some(3),
            "the local task advanced its Rc counter"
        );
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn a_local_task_holds_rc_state_across_sleeps() {
    let report = SimulationBuilder::new()
        .processes(2, || Box::new(RcCounterProcess))
        .workload(PatientWorkload)
        .set_iterations(3)
        .set_debug_seeds(vec![1, 2, 3])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "report: {report:?}");
    assert!(
        report.assertion_violations.is_empty(),
        "{:?}",
        report.assertion_violations
    );
    let advanced = report
        .assertion_results
        .get("the local task advanced its Rc counter")
        .map_or(0, |stats| stats.successes);
    assert!(advanced > 0, "the counter assertion never ran");
}

// ---------------------------------------------------------------------------
// b. Local and Send tasks interleave on one seeded queue, deterministically.
// ---------------------------------------------------------------------------

/// Two local tasks and one `Send` task race on identical deadlines and
/// yields, recording every step.
struct InterleavingWorkload {
    trace: Arc<Mutex<Vec<String>>>,
}

fn record(trace: &Mutex<Vec<String>>, entry: String) {
    trace
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .push(entry);
}

#[async_trait]
impl Workload for InterleavingWorkload {
    fn name(&self) -> &'static str {
        "interleaving"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let mut handles = Vec::new();
        for i in 0..2u32 {
            let time = ctx.time().clone();
            let tasks = ctx.task().clone();
            let trace = Arc::clone(&self.trace);
            handles.push(ctx.task().spawn_local(&format!("local-{i}"), async move {
                let rounds = Rc::new(Cell::new(0u32));
                while rounds.get() < 3 {
                    let _ = time.sleep(Duration::from_millis(10)).await;
                    tasks.yield_now().await;
                    record(
                        &trace,
                        format!("local-{i} round{} t={:?}", rounds.get(), time.now()),
                    );
                    rounds.set(rounds.get() + 1);
                }
            }));
        }
        let time = ctx.time().clone();
        let tasks = ctx.task().clone();
        let trace = Arc::clone(&self.trace);
        handles.push(ctx.task().spawn_task("send-0", async move {
            for round in 0..3u32 {
                let _ = time.sleep(Duration::from_millis(10)).await;
                tasks.yield_now().await;
                record(&trace, format!("send-0 round{round} t={:?}", time.now()));
            }
        }));
        for handle in handles {
            handle.await.map_err(|e| join_failed(&e))?;
        }
        Ok(())
    }
}

fn interleaving_trace(seed: u64) -> Vec<String> {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let report = SimulationBuilder::new()
        .workload(InterleavingWorkload {
            trace: Arc::clone(&trace),
        })
        .set_debug_seeds(vec![seed])
        .set_iterations(1)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "report: {report:?}");
    trace
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .clone()
}

#[test]
fn local_and_send_tasks_replay_identically() {
    for seed in [17, 4242] {
        let first = interleaving_trace(seed);
        let second = interleaving_trace(seed);
        assert_eq!(first.len(), 9, "every task recorded every round");
        assert_eq!(
            first, second,
            "seed {seed} must replay the same interleaving"
        );
    }
}

#[test]
fn local_tasks_run_across_many_seeds() {
    let report = SimulationBuilder::new()
        .workload(InterleavingWorkload {
            trace: Arc::new(Mutex::new(Vec::new())),
        })
        .set_iterations(20)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.iterations, 20);
    assert_eq!(report.failed_runs, 0, "report: {report:?}");
}

// ---------------------------------------------------------------------------
// c. A crash drops the process's local tasks.
// ---------------------------------------------------------------------------

/// Spawns a detached local worker that ticks an `Rc<Cell<u64>>` every 10ms
/// and publishes it, then parks until shut down.
struct LocalWorkerProcess;

#[async_trait]
impl Process for LocalWorkerProcess {
    fn name(&self) -> &'static str {
        "local_worker_process"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let key = format!("local_worker_ticks:{}", ctx.my_ip());
        let time = ctx.time().clone();
        let state = ctx.state().clone();
        ctx.task()
            .spawn_local("local-worker", async move {
                // Resume from the published count so the restart check sees
                // the new boot's ticks as growth.
                let ticks = Rc::new(Cell::new(state.get::<u64>(&key).unwrap_or(0)));
                loop {
                    if time.sleep(Duration::from_millis(10)).await.is_err() {
                        return;
                    }
                    ticks.set(ticks.get() + 1);
                    state.publish(&key, ticks.get());
                }
            })
            .detach();
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Crash process 0, hold it down, and require its local worker to have
/// stopped; then restart it and require a worker to tick again.
struct CrashAndWatchLocalWorker;

impl CrashAndWatchLocalWorker {
    async fn pause(ctx: &FaultContext, ms: u64) -> SimulationResult<bool> {
        if ctx.chaos_shutdown().is_cancelled() {
            return Ok(false);
        }
        ctx.time()
            .sleep(Duration::from_millis(ms))
            .await
            .map_err(|e| sleep_failed(&e))?;
        Ok(!ctx.chaos_shutdown().is_cancelled())
    }
}

#[async_trait]
impl FaultInjector for CrashAndWatchLocalWorker {
    fn name(&self) -> &'static str {
        "crash_and_watch_local_worker"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        if !Self::pause(ctx, 100).await? {
            return Ok(());
        }
        let target = ctx.process_ips()[0].clone();
        let key = format!("local_worker_ticks:{target}");
        assert_always!(
            ctx.state().get::<u64>(&key).unwrap_or(0) > 0,
            "the local worker ticked before the crash"
        );

        ctx.crash(&target)?;
        if !Self::pause(ctx, 30).await? {
            return Ok(());
        }
        let held: u64 = ctx.state().get(&key).unwrap_or(0);
        if !Self::pause(ctx, 200).await? {
            return Ok(());
        }
        assert_always!(
            ctx.state().get::<u64>(&key).unwrap_or(0) == held,
            "a crashed process's local tasks must stop with it"
        );

        ctx.restart(&target)?;
        let mut recovered = false;
        for _ in 0..20 {
            if !Self::pause(ctx, 20).await? {
                break;
            }
            if ctx.state().get::<u64>(&key).unwrap_or(0) > held {
                recovered = true;
                break;
            }
        }
        assert_always!(
            recovered,
            "the restarted process's local worker ticks again"
        );
        Ok(())
    }
}

#[test]
fn a_crash_stops_the_local_tasks_the_process_spawned() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(LocalWorkerProcess))
        .workload_factory(|| Box::new(PatientWorkload))
        .fault_factory(|| Box::new(CrashAndWatchLocalWorker))
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(3)
        .set_debug_seeds(vec![7, 11, 13])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.iterations, 3);
    assert!(
        report.assertion_violations.is_empty(),
        "{:?}",
        report.assertion_violations
    );
    assert_eq!(
        report.failed_runs, 0,
        "no local task may outlive its process's crash"
    );
    let stopped = report
        .assertion_results
        .get("a crashed process's local tasks must stop with it")
        .map_or(0, |stats| stats.successes);
    assert_eq!(stopped, 3, "every seed checked the held-down worker");
}

// ---------------------------------------------------------------------------
// d. Local task panics are reported like spawn_task panics.
// ---------------------------------------------------------------------------

/// Detaches a local child that panics, then parks.
struct ProcessLocalChildPanic;

#[async_trait]
impl Process for ProcessLocalChildPanic {
    fn name(&self) -> &'static str {
        "process_local_child_panic"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.task()
            .spawn_local("local-child", async {
                let state = Rc::new(());
                let _held = Rc::clone(&state);
                panic!("local child panic");
            })
            .detach();
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

#[test]
fn local_child_panic_fails_the_seed() {
    let seen = Arc::new(Mutex::new(Vec::<TraceEvent>::new()));
    let seen_invariant = Arc::clone(&seen);
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(ProcessLocalChildPanic))
        .workload(PatientWorkload)
        .invariant_fn("local panic diagnostics", move |q, _| {
            *seen_invariant.lock().expect("seen lock poisoned") =
                q.snapshot("unobserved_task_panic");
        })
        .set_iterations(1)
        .set_debug_seeds(vec![3])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.successful_runs, 0, "report: {report:?}");
    assert_eq!(report.failed_runs, 1, "report: {report:?}");
    let seen = seen.lock().expect("seen lock poisoned");
    assert_eq!(seen.len(), 1, "the panic should reach the final pass");
    let event = &seen[0];
    assert_eq!(event.source, "sim");
    assert_eq!(event.str("task"), Some("local-child"));
    assert_eq!(event.str("panic"), Some("local child panic"));
}

/// Joins a local child that panics and handles the error.
struct HandledJoinedLocalPanic;

#[async_trait]
impl Workload for HandledJoinedLocalPanic {
    fn name(&self) -> &'static str {
        "handled_joined_local_panic"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let joined = ctx.task().spawn_local("joined-local-child", async {
            let _held = Rc::new(());
            panic!("joined local child panic");
        });
        match joined.await {
            Err(JoinError::Panicked) => Ok(()),
            other => Err(SimulationError::InvalidState(format!(
                "expected a recoverable joined panic, got {other:?}"
            ))),
        }
    }
}

#[test]
fn handled_joined_local_panic_does_not_fail_the_seed() {
    let report = SimulationBuilder::new()
        .workload(HandledJoinedLocalPanic)
        .set_iterations(1)
        .set_debug_seeds(vec![8])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.successful_runs, 1, "report: {report:?}");
    assert_eq!(report.failed_runs, 0, "report: {report:?}");
}

// ---------------------------------------------------------------------------
// e. timeout_local bounds an Rc-holding future, both outcomes reached.
// ---------------------------------------------------------------------------

/// Races an `Rc`-holding sleep of seeded length against a fixed deadline.
struct TimeoutLocalWorkload;

#[async_trait]
impl Workload for TimeoutLocalWorkload {
    fn name(&self) -> &'static str {
        "timeout_local"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let time = ctx.time().clone();
        let delay_ms = ctx.random().random_range(0..20u64);
        ctx.task()
            .spawn_local("timeout-local", async move {
                let finished = Rc::new(Cell::new(false));
                let held = Rc::clone(&finished);
                let inner_time = time.clone();
                let inner = async move {
                    let _ = inner_time.sleep(Duration::from_millis(delay_ms)).await;
                    held.set(true);
                };
                let outcome = time.timeout_local(Duration::from_millis(10), inner).await;
                assert_always!(
                    outcome.is_ok() == finished.get(),
                    "timeout_local reports completion iff the future finished"
                );
                assert_sometimes!(outcome.is_ok(), "timeout_local completed");
                assert_sometimes!(
                    matches!(outcome, Err(TimeError::Elapsed)),
                    "timeout_local elapsed"
                );
            })
            .await
            .map_err(|e| join_failed(&e))
    }
}

#[test]
fn timeout_local_reaches_both_outcomes() {
    let report = SimulationBuilder::new()
        .workload(TimeoutLocalWorkload)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "report: {report:?}");
    assert!(
        report.assertion_violations.is_empty(),
        "{:?}",
        report.assertion_violations
    );
    assert!(
        report.coverage_violations.is_empty(),
        "{:?}",
        report.coverage_violations
    );
    for name in ["timeout_local completed", "timeout_local elapsed"] {
        let reached = report
            .assertion_results
            .get(name)
            .map_or(0, |stats| stats.successes);
        assert!(reached > 0, "{name} was never reached");
    }
}
