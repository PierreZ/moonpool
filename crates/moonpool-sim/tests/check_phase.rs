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
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.iterations, 1);
    assert_eq!(
        report.successful_runs, 0,
        "a rejected check is not a success"
    );
    assert_eq!(report.failed_runs, 1, "a rejected check fails the seed");
    assert_eq!(report.seeds_failing, vec![1]);
}

/// `run()` fails with an error of its own.
struct RunRejects;

#[async_trait]
impl Workload for RunRejects {
    fn name(&self) -> &'static str {
        "run_rejects"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Err(SimulationError::InvalidState(
            "run() gave up on key 3".to_string(),
        ))
    }
}

/// The report keeps the cause of a failed seed and prints it next to the
/// seed (#458).
#[test]
fn the_report_keeps_the_cause_of_a_failed_seed() {
    let report = SimulationBuilder::new()
        .workload(CheckRejects)
        .workload(RunRejects)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run()
        .expect("simulation configuration is valid");

    let cause = report
        .failure_causes
        .get(&1)
        .unwrap_or_else(|| panic!("no cause for seed 1: {report:?}"));
    assert!(
        cause.contains("final state rejected by check"),
        "the check error is the cause: {cause}"
    );
    assert!(
        cause.contains("run() gave up on key 3"),
        "the run error is the cause too: {cause}"
    );
    let individual = report.individual_metrics[0]
        .as_ref()
        .expect_err("the seed failed");
    assert!(
        individual
            .to_string()
            .contains("final state rejected by check"),
        "the iteration result keeps the cause: {individual}"
    );
    let text = report.to_string();
    assert!(
        text.contains("Seed 1: ") && text.contains("final state rejected by check"),
        "the report prints the cause next to the seed: {text}"
    );
}

#[test]
fn check_panic_fails_the_iteration() {
    let report = SimulationBuilder::new()
        .workload(CheckPanics)
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .run()
        .expect("simulation configuration is valid");

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
        .run()
        .expect("simulation configuration is valid");

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
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(report.iterations, 1);
    assert_eq!(report.successful_runs, 1);
    assert_eq!(report.failed_runs, 0);
}
