//! Setup and check phases use the simulation virtual-time stall guard.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationBuilder, SimulationResult, TimeProvider, Workload};

async fn ignore_shutdown_sleep_loop(ctx: &SimContext) -> SimulationResult<()> {
    loop {
        let _ = ctx.time().sleep(Duration::from_secs(1)).await;
    }
}

struct SetupTimerLoop;

#[async_trait]
impl Workload for SetupTimerLoop {
    fn name(&self) -> &'static str {
        "setup_timer_loop"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ignore_shutdown_sleep_loop(ctx).await
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}

#[test]
fn setup_timer_loop_fails_instead_of_hanging() {
    let report = SimulationBuilder::new()
        .workload(SetupTimerLoop)
        .run_time_budget(Duration::from_secs(1))
        .set_iterations(1)
        .set_debug_seeds(vec![11])
        .run();

    assert_eq!(report.failed_runs, 1, "report: {report:?}");
    assert_eq!(report.seeds_failing, vec![11], "report: {report:?}");
}

struct CheckTimerLoop;

#[async_trait]
impl Workload for CheckTimerLoop {
    fn name(&self) -> &'static str {
        "check_timer_loop"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ignore_shutdown_sleep_loop(ctx).await
    }
}

#[test]
fn check_timer_loop_fails_instead_of_hanging() {
    let report = SimulationBuilder::new()
        .workload(CheckTimerLoop)
        .run_time_budget(Duration::from_secs(1))
        .set_iterations(1)
        .set_debug_seeds(vec![12])
        .run();

    assert_eq!(report.failed_runs, 1, "report: {report:?}");
    assert_eq!(report.seeds_failing, vec![12], "report: {report:?}");
}
