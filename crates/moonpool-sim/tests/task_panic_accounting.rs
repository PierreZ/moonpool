//! Detached actor tasks that panic must fail their simulation seed.

use std::future;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    FaultContext, FaultInjector, Process, SimContext, SimulationBuilder, SimulationResult,
    TaskProvider, TimeProvider, Workload,
};

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
        .run();

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
        .run();

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
        .run();

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
        .run();

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
        .run();

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
        .run();

    assert_eq!(report.failed_runs, 0, "report: {report:?}");
    assert_eq!(report.successful_runs, 1, "report: {report:?}");
}
