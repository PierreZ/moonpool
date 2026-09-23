//! Integration tests for the `IterationControl::UntilCoverageStable` stop condition.
//!
//! These exercise the runner's ability to terminate early when every observed
//! `assert_sometimes!` / `assert_reachable!` has fired and coverage stops
//! growing, both with and without fork-based exploration enabled. Under
//! `cargo nextest` the binary is not sancov-instrumented, so these run on the
//! assertion-coverage fallback signal.

use async_trait::async_trait;
#[cfg(feature = "exploration")]
use moonpool_sim::ExplorationConfig;
use moonpool_sim::{SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload};

fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    builder.run()
}

/// Workload firing three sometimes assertions deterministically. The full
/// coverage surface is exhausted after the first seed.
struct ThreeAlwaysHitWorkload;

#[async_trait]
impl Workload for ThreeAlwaysHitWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, "gate 1");
        moonpool_sim::assert_sometimes!(true, "gate 2");
        moonpool_sim::assert_sometimes!(true, "gate 3");
        Ok(())
    }
}

/// Workload whose third assertion only fires in roughly 1-in-5 seeds.
/// Saturation must refuse to stop until every sometimes has hit.
struct RareGateWorkload;

#[async_trait]
impl Workload for RareGateWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, "gate 1");
        moonpool_sim::assert_sometimes!(true, "gate 2");
        let rare = moonpool_sim::sim_random_range(0u32..5) == 0;
        moonpool_sim::assert_sometimes!(rare, "rare gate");
        Ok(())
    }
}

/// A numeric coverage gate that never succeeds beside an ordinary gate that
/// always does. Numeric coverage must prevent early convergence too.
struct NeverHitNumericGate;

#[async_trait]
impl Workload for NeverHitNumericGate {
    fn name(&self) -> &'static str {
        "numeric_client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, "boolean gate");
        moonpool_sim::assert_sometimes_greater_than!(0, 1, "numeric gate");
        Ok(())
    }
}

/// With no exploration enabled, saturation should still fire because the
/// assertion table is initialised unconditionally.
#[test]
fn test_plateau_no_exploration() {
    let report = run_simulation(
        SimulationBuilder::new()
            .until_coverage_stable(3, 100)
            .workload(ThreeAlwaysHitWorkload),
    );

    assert!(
        report.iterations < 100,
        "plateau should stop early; ran {} iterations",
        report.iterations
    );
    assert!(
        report.iterations >= 4,
        "need at least seed 1 + 3 plateau seeds; got {}",
        report.iterations
    );
    assert!(
        !report.convergence_timeout,
        "should not be marked as a convergence timeout"
    );
    assert_eq!(report.failed_runs, 0);
}

/// A workload with no coverage assertion at all.
struct NoCoverageAssertions;

#[async_trait]
impl Workload for NoCoverageAssertions {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}

/// The default stop condition must converge on the plateau alone when the
/// simulation declares no `assert_sometimes!` / `assert_reachable!`: there is
/// nothing left to reach, so it must not run to the cap and time out.
#[test]
fn plateau_alone_converges_without_coverage_assertions() {
    let report = run_simulation(
        SimulationBuilder::new()
            .until_coverage_stable(3, 100)
            .workload(NoCoverageAssertions),
    );

    assert_eq!(
        report.iterations, 4,
        "seed 1 plus 3 quiet plateau seeds, then stop"
    );
    assert!(!report.convergence_timeout);
    assert_eq!(report.failed_runs, 0);
    let saturation = report.saturation.expect("saturation is reported");
    assert_eq!(saturation.sometimes_total, 0);
}

/// Same condition with exploration enabled — children write into the shared
/// assertion table, so the plateau accumulator sees the full coverage too.
#[cfg(feature = "exploration")]
#[test]
fn test_plateau_with_exploration() {
    let report = run_simulation(
        SimulationBuilder::new()
            .enable_exploration(ExplorationConfig {
                workers: 0,
                max_runs_per_seed: 20,
                branching_factor: 2,
                max_frontier: 64,
                max_recipe_len: 8,
            })
            .until_coverage_stable(3, 100)
            .workload_factory(|| Box::new(ThreeAlwaysHitWorkload)),
    );

    assert!(
        report.iterations < 100,
        "plateau should stop early; ran {} iterations",
        report.iterations
    );
    assert!(
        !report.convergence_timeout,
        "should not be marked as a convergence timeout"
    );
    assert_eq!(report.failed_runs, 0);
}

/// All-sometimes is always required, so the runner must keep going until the
/// rare gate has fired even if the other two assertions plateaued early.
#[test]
fn test_plateau_requires_all_sometimes() {
    let report = run_simulation(
        SimulationBuilder::new()
            .until_coverage_stable(2, 200)
            .workload(RareGateWorkload),
    );

    assert!(
        !report.convergence_timeout,
        "rare gate should fire within 200 seeds; got convergence_timeout={}",
        report.convergence_timeout
    );

    let rare_hit = report
        .assertion_details
        .iter()
        .any(|d| d.msg == "rare gate" && d.pass_count > 0);
    assert!(
        rare_hit,
        "expected the rare gate to have fired before stopping"
    );
}

#[test]
fn numeric_sometimes_prevents_early_convergence() {
    let report = run_simulation(
        SimulationBuilder::new()
            .until_coverage_stable(2, 5)
            .workload(NeverHitNumericGate),
    );

    assert_eq!(report.iterations, 5, "report: {report:?}");
    assert!(report.convergence_timeout, "report: {report:?}");
    assert!(
        report
            .coverage_violations
            .iter()
            .any(|violation| violation.contains("numeric gate")),
        "report: {report:?}"
    );
}
