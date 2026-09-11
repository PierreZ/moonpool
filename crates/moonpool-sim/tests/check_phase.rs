//! Regression: a `Workload::check()` verdict is part of the seed's result.
//!
//! The check phase used to log a failing `check()` and discard it, and a
//! `check()` that panicked was logged and its workload silently omitted.
//! Either way the report kept the run phase's results unchanged, so a final
//! consistency check could explicitly reject a run while CI received
//! `successful_runs = 1, failed_runs = 0`.

use async_trait::async_trait;

use moonpool_sim::{SimContext, SimulationBuilder, SimulationError, SimulationResult, Workload};

/// `run()` succeeds, `check()` rejects the final state.
struct CheckRejects;

#[async_trait]
impl Workload for CheckRejects {
    fn name(&self) -> &'static str {
        "check_rejects"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Err(SimulationError::InvalidState(
            "final state rejected by check".to_string(),
        ))
    }
}

/// `run()` succeeds, `check()` panics (an ordinary assertion failure).
struct CheckPanics;

#[async_trait]
impl Workload for CheckPanics {
    fn name(&self) -> &'static str {
        "check_panics"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        panic!("check phase assertion failure");
    }
}

/// Neither `run()` nor `check()` complains; the control case.
struct CheckPasses;

#[async_trait]
impl Workload for CheckPasses {
    fn name(&self) -> &'static str {
        "check_passes"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}

#[test]
fn check_error_fails_the_iteration() {
    let report = SimulationBuilder::new()
        .workload(CheckRejects)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(
        report.successful_runs, 0,
        "a rejected check is not a success"
    );
    assert_eq!(report.failed_runs, 1, "a rejected check fails the seed");
    assert_eq!(report.seeds_failing, vec![1]);
}

#[test]
fn check_panic_fails_the_iteration() {
    let report = SimulationBuilder::new()
        .workload(CheckPanics)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(
        report.successful_runs, 0,
        "a panicking check is not a success"
    );
    assert_eq!(report.failed_runs, 1, "a panicking check fails the seed");
    assert_eq!(report.seeds_failing, vec![1]);
}

#[test]
fn one_rejecting_check_fails_the_seed_for_all_workloads() {
    let report = SimulationBuilder::new()
        .workload(CheckPasses)
        .workload(CheckRejects)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(report.successful_runs, 0);
    assert_eq!(report.failed_runs, 1, "one failing check is enough");
}

#[test]
fn passing_check_stays_a_success() {
    let report = SimulationBuilder::new()
        .workload(CheckPasses)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run();

    assert_eq!(report.iterations, 1);
    assert_eq!(report.successful_runs, 1);
    assert_eq!(report.failed_runs, 0);
}
