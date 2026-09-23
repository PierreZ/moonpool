//! A re-armed zero-duration provider sleep cannot hold a seed forever.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationBuilder, SimulationResult, TimeProvider, Workload};

struct ZeroSleepLoop;

#[async_trait]
impl Workload for ZeroSleepLoop {
    fn name(&self) -> &'static str {
        "zero_sleep_loop"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        loop {
            let _ = ctx.time().sleep(Duration::ZERO).await;
        }
    }
}

#[test]
fn zero_sleep_loop_fails_instead_of_hanging() {
    let report = SimulationBuilder::new()
        .workload(ZeroSleepLoop)
        .set_iterations(1)
        .set_debug_seeds(vec![13])
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.failed_runs, 1, "report: {report:?}");
    assert_eq!(report.seeds_failing, vec![13], "report: {report:?}");
}
