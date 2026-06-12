//! Integration tests for the `IterationControl::CoveragePlateau` stop condition.
//!
//! These exercise the runner's ability to terminate early when
//! `assert_sometimes!` / `assert_reachable!` coverage stops growing,
//! both with and without fork-based exploration enabled.

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
/// `require_all_sometimes=true` should refuse to stop until it has hit.
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

/// With no exploration enabled, plateau detection should still fire because
/// the assertion table is initialised unconditionally.
#[test]
fn test_plateau_no_exploration() {
    let report = run_simulation(
        SimulationBuilder::new()
            .until_coverage_plateau(3, 100)
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

/// Same condition with exploration enabled — children write into the shared
/// assertion table, so the plateau accumulator sees the full coverage too.
#[cfg(feature = "exploration")]
#[test]
fn test_plateau_with_exploration() {
    let report = run_simulation(
        SimulationBuilder::new()
            .enable_exploration(ExplorationConfig {
                max_depth: 1,
                timelines_per_split: 2,
                global_energy: 20,
                adaptive: None,
                parallelism: None,
            })
            .until_coverage_plateau(3, 100)
            .workload(ThreeAlwaysHitWorkload),
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

/// With `require_all_sometimes=true`, the runner must keep going until the
/// rare gate has fired even if the other two assertions plateaued early.
#[test]
fn test_plateau_requires_all_sometimes() {
    let report = run_simulation(
        SimulationBuilder::new()
            .until_coverage_plateau_with(2, true, 200)
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
