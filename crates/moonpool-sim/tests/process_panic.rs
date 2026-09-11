//! Regression: a panic inside `Process::run` fails the seed.
//!
//! The executor catches a task's panic and parks it in the join result, but
//! nothing ever awaited a process handle: the process manager aborts and
//! drops them. So a server that panicked on its first poll, beside a workload
//! that slept and returned `Ok`, produced a successful simulation report — an
//! assertion in a dependency could disappear from failure accounting unless
//! a workload independently noticed the dead server.

use std::time::Duration;

use async_trait::async_trait;

use moonpool_sim::{
    Process, SimContext, SimulationBuilder, SimulationError, SimulationResult, TimeProvider,
    Workload,
};

/// Panics as soon as it is polled.
struct PanickingProcess;

#[async_trait]
impl Process for PanickingProcess {
    fn name(&self) -> &'static str {
        "panicking_process"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        panic!("server assertion failed");
    }
}

/// Returns an error, which is an ordinary exit and not a failure.
struct ExitingProcess;

#[async_trait]
impl Process for ExitingProcess {
    fn name(&self) -> &'static str {
        "exiting_process"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Err(SimulationError::InvalidState(
            "server chose to stop".to_string(),
        ))
    }
}

/// Never looks at the servers; sleeps and reports success.
struct ObliviousWorkload;

#[async_trait]
impl Workload for ObliviousWorkload {
    fn name(&self) -> &'static str {
        "oblivious_workload"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(Duration::from_millis(50)).await;
        Ok(())
    }
}

#[test]
fn a_panicking_process_fails_the_iteration() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(PanickingProcess))
        .workload(ObliviousWorkload)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(
        report.successful_runs, 0,
        "a panicking server is not a success"
    );
    assert_eq!(report.failed_runs, 1, "a panicking server fails the seed");
    assert_eq!(report.seeds_failing, vec![1]);
}

#[test]
fn a_process_that_exits_with_an_error_is_not_a_failure() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(ExitingProcess))
        .workload(ObliviousWorkload)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(report.successful_runs, 1);
    assert_eq!(report.failed_runs, 0);
}
