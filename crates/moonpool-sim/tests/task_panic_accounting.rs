//! Detached actor tasks that panic must fail their simulation seed.

use std::future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::JoinError;
use moonpool_sim::{
    FaultContext, FaultInjector, Process, SimContext, SimulationBuilder, SimulationError,
    SimulationResult, TaskProvider, TimeProvider, TraceEvent, Workload,
};

struct StalledSetupAfterChildPanic {
    actor: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl Workload for StalledSetupAfterChildPanic {
    fn name(&self) -> &'static str {
        "stalled_setup_after_child_panic"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        *self.actor.lock().expect("actor lock poisoned") =
            Some(format!("workload@{}", ctx.my_ip()));
        ctx.task()
            .spawn_task("stalled-setup-child", async {
                panic!("stalled setup child panic");
            })
            .detach();
        loop {
            let _ = ctx.time().sleep(Duration::from_secs(1)).await;
        }
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}

#[test]
fn fatal_setup_stall_keeps_unobserved_panic_diagnostics() {
    let actor = Arc::new(Mutex::new(None));
    let seen = Arc::new(Mutex::new((
        Vec::<TraceEvent>::new(),
        Vec::<TraceEvent>::new(),
        0_usize,
    )));
    let seen_invariant = Arc::clone(&seen);
    let report = SimulationBuilder::new()
        .workload(StalledSetupAfterChildPanic {
            actor: Arc::clone(&actor),
        })
        .invariant_fn("panic diagnostics", move |q, _| {
            tracing::info!("unrelated_outside_actor");
            *seen_invariant.lock().expect("seen lock poisoned") = (
                q.snapshot("unobserved_task_panic"),
                q.snapshot("workload_task_lost"),
                q.len("unrelated_outside_actor"),
            );
        })
        .run_time_budget(Duration::from_secs(1))
        .set_iterations(1)
        .set_debug_seeds(vec![12])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 1, "report: {report:?}");
    assert_eq!(report.seeds_failing, vec![12], "report: {report:?}");
    let actor = actor
        .lock()
        .expect("actor lock poisoned")
        .clone()
        .expect("setup must record its actor");
    let seen = seen.lock().expect("seen lock poisoned");
    assert_eq!(
        seen.2, 0,
        "unrelated events outside actor spans are dropped"
    );
    assert_eq!(seen.0.len(), 1, "the panic should reach the final pass");
    let event = &seen.0[0];
    assert_eq!(event.source, "sim");
    assert_eq!(event.name, "unobserved_task_panic");
    assert_eq!(event.str("actor"), Some(actor.as_str()));
    assert_eq!(event.str("task"), Some("stalled-setup-child"));
    assert_eq!(event.str("panic"), Some("stalled setup child panic"));
    assert_eq!(seen.1.len(), 1, "the lost workload should be observed");
    let event = &seen.1[0];
    assert_eq!(event.source, "10.0.0.1");
    assert_eq!(event.name, "workload_task_lost");
    assert_eq!(event.str("phase"), Some("setup"));
    assert_eq!(event.str("cause"), Some("task was cancelled"));
}

struct HandledJoinedPanic;

#[async_trait]
impl Workload for HandledJoinedPanic {
    fn name(&self) -> &'static str {
        "handled_joined_panic"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let joined = ctx.task().spawn_task("joined-child", async {
            panic!("joined child panic");
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
fn handled_joined_panic_does_not_fail_the_seed() {
    let report = SimulationBuilder::new()
        .workload(HandledJoinedPanic)
        .set_iterations(1)
        .set_debug_seeds(vec![8])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.successful_runs, 1, "report: {report:?}");
    assert_eq!(report.failed_runs, 0, "report: {report:?}");
}

struct CompletedUnobservedPanic {
    detach: bool,
}

#[async_trait]
impl Workload for CompletedUnobservedPanic {
    fn name(&self) -> &'static str {
        "completed_unobserved_panic"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let handle = ctx.task().spawn_task("completed-child", async {
            panic!("completed child panic");
        });
        ctx.task().yield_now().await;
        if !handle.is_finished() {
            return Err(SimulationError::InvalidState(
                "child did not panic before detachment".to_string(),
            ));
        }
        if self.detach {
            handle.detach();
        } else {
            drop(handle);
        }
        Ok(())
    }
}

#[test]
fn panic_before_explicit_detach_still_fails_the_seed() {
    let report = SimulationBuilder::new()
        .workload(CompletedUnobservedPanic { detach: true })
        .set_iterations(1)
        .set_debug_seeds(vec![9])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.successful_runs, 0, "report: {report:?}");
    assert_eq!(report.failed_runs, 1, "report: {report:?}");
}

#[test]
fn panic_before_handle_drop_still_fails_the_seed() {
    let report = SimulationBuilder::new()
        .workload(CompletedUnobservedPanic { detach: false })
        .set_iterations(1)
        .set_debug_seeds(vec![10])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.successful_runs, 0, "report: {report:?}");
    assert_eq!(report.failed_runs, 1, "report: {report:?}");
}

struct AbortedBeforePanic;

#[async_trait]
impl Workload for AbortedBeforePanic {
    fn name(&self) -> &'static str {
        "aborted_before_panic"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let handle = ctx.task().spawn_task("aborted-child", async {
            panic!("must never be polled");
        });
        handle.abort();
        match handle.await {
            Err(JoinError::Cancelled) => Ok(()),
            other => Err(SimulationError::InvalidState(format!(
                "expected cancellation before panic, got {other:?}"
            ))),
        }
    }
}

#[test]
fn abort_before_child_poll_is_not_a_panic() {
    let report = SimulationBuilder::new()
        .workload(AbortedBeforePanic)
        .set_iterations(1)
        .set_debug_seeds(vec![11])
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.successful_runs, 1, "report: {report:?}");
    assert_eq!(report.failed_runs, 0, "report: {report:?}");
}

struct SetupChildPanic;

#[async_trait]
impl Workload for SetupChildPanic {
    fn name(&self) -> &'static str {
        "setup_child_panic"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.task()
            .spawn_task("setup-child", async { panic!("setup child panic") })
            .detach();
        ctx.time()
            .sleep(Duration::from_millis(1))
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("sleep failed: {e}"))
            })?;
        Ok(())
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}

#[test]
fn setup_child_panic_fails_the_seed() {
    let report = SimulationBuilder::new()
        .workload(SetupChildPanic)
        .set_iterations(1)
        .set_debug_seeds(vec![1])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.successful_runs, 0, "report: {report:?}");
    assert_eq!(report.failed_runs, 1, "report: {report:?}");
}

struct CheckChildPanic;

#[async_trait]
impl Workload for CheckChildPanic {
    fn name(&self) -> &'static str {
        "check_child_panic"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.task()
            .spawn_task("check-child", async { panic!("check child panic") })
            .detach();
        ctx.time()
            .sleep(Duration::from_millis(1))
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("sleep failed: {e}"))
            })?;
        Ok(())
    }
}

#[test]
fn check_child_panic_fails_the_seed() {
    let report = SimulationBuilder::new()
        .workload(CheckChildPanic)
        .set_iterations(1)
        .set_debug_seeds(vec![2])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.successful_runs, 0, "report: {report:?}");
    assert_eq!(report.failed_runs, 1, "report: {report:?}");
}

struct ProcessChildPanic;

#[async_trait]
impl Process for ProcessChildPanic {
    fn name(&self) -> &'static str {
        "process_child_panic"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.task()
            .spawn_task("process-child", async { panic!("process child panic") })
            .detach();
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

struct BriefWorkload;

#[async_trait]
impl Workload for BriefWorkload {
    fn name(&self) -> &'static str {
        "brief"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(Duration::from_millis(1))
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("sleep failed: {e}"))
            })?;
        Ok(())
    }
}

#[test]
fn process_child_panic_fails_the_seed() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(ProcessChildPanic))
        .workload(BriefWorkload)
        .set_iterations(1)
        .set_debug_seeds(vec![3])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.successful_runs, 0, "report: {report:?}");
    assert_eq!(report.failed_runs, 1, "report: {report:?}");
}

struct PanickingFault;

#[async_trait]
impl FaultInjector for PanickingFault {
    fn name(&self) -> &'static str {
        "panicking_fault"
    }

    async fn inject(&mut self, _ctx: &FaultContext) -> SimulationResult<()> {
        panic!("fault injector panic");
    }
}

#[test]
fn fault_injector_panic_fails_the_seed() {
    let report = SimulationBuilder::new()
        .fault_factory(|| Box::new(PanickingFault))
        .chaos_duration(Duration::from_millis(10))
        .workload(BriefWorkload)
        .set_iterations(1)
        .set_debug_seeds(vec![4])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.successful_runs, 0, "report: {report:?}");
    assert_eq!(report.failed_runs, 1, "report: {report:?}");
}

struct FirstIterationPanic {
    panic_once: bool,
}

#[async_trait]
impl Workload for FirstIterationPanic {
    fn name(&self) -> &'static str {
        "first_iteration_panic"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        if self.panic_once {
            self.panic_once = false;
            ctx.task()
                .spawn_task("first-child", async { panic!("first iteration panic") })
                .detach();
            ctx.time()
                .sleep(Duration::from_millis(1))
                .await
                .map_err(|e| {
                    moonpool_sim::SimulationError::InvalidState(format!("sleep failed: {e}"))
                })?;
        }
        Ok(())
    }
}

#[test]
fn task_panic_accounting_is_reset_for_each_seed() {
    let report = SimulationBuilder::new()
        .workload(FirstIterationPanic { panic_once: true })
        .set_iterations(2)
        .set_debug_seeds(vec![5, 6])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 1, "report: {report:?}");
    assert_eq!(report.successful_runs, 1, "report: {report:?}");
    assert_eq!(report.seeds_failing, vec![5], "report: {report:?}");
}

struct DetachedCancellation;

#[async_trait]
impl Workload for DetachedCancellation {
    fn name(&self) -> &'static str {
        "detached_cancellation"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.task().spawn_task("never", future::pending()).detach();
        Ok(())
    }
}

#[test]
fn executor_teardown_cancellation_is_not_a_task_panic() {
    let report = SimulationBuilder::new()
        .workload(DetachedCancellation)
        .set_iterations(1)
        .set_debug_seeds(vec![7])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 0, "report: {report:?}");
    assert_eq!(report.successful_runs, 1, "report: {report:?}");
}
