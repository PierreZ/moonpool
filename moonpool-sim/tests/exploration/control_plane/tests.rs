//! Test functions for the control-plane exploration tests.

use moonpool_sim::{ExplorationConfig, SimulationBuilder, SimulationReport};

use super::broker::BrokerWorkload;
use super::cp::ControlPlaneWorkload;
use super::driver::DriverWorkload;
use super::node::NodeWorkload;

/// Helper to run a simulation and return the report.
fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

/// Quick debug test: single iteration, no exploration, no chaos.
#[test]
fn test_control_plane_basic() {
    let report = run_simulation(
        SimulationBuilder::new()
            .workload(BrokerWorkload::new())
            .workload(ControlPlaneWorkload::new())
            .workload(NodeWorkload::new())
            .workload(DriverWorkload::new())
            .set_iterations(1)
            .set_debug_seeds(vec![42]),
    );

    eprintln!("{}", report);
    assert_eq!(report.successful_runs, 1);
}

/// Control-plane exploration test: fork-based exploration finds consistency
/// bugs caused by a flaky message broker.
///
/// The broker has a P=0.10 bug that returns errors while internally succeeding,
/// causing divergence between ControlPlane's VM registry and Node's actual VMs.
/// Exploration amplifies rare error paths to reliably discover the bug.
///
/// Uses multi-seed exploration (2 iterations) for coverage-preserving discovery.
#[test]
fn slow_simulation_control_plane() {
    let report = run_simulation(
        SimulationBuilder::new()
            .workload(BrokerWorkload::new())
            .workload(ControlPlaneWorkload::new())
            .workload(NodeWorkload::new())
            .workload(DriverWorkload::new())
            .random_network()
            .set_iterations(2)
            .enable_exploration(ExplorationConfig {
                max_depth: 60,
                timelines_per_split: 4,
                global_energy: 100_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 20,
                    min_timelines: 200,
                    max_timelines: 500,
                    per_mark_energy: 5_000,
                    warm_min_timelines: Some(20),
                }),
                parallelism: Some(moonpool_sim::Parallelism::HalfCores),
            }),
    );

    // All parent runs should succeed
    assert_eq!(report.successful_runs, 2);

    let exp = report.exploration.expect("exploration report missing");
    assert!(exp.total_timelines > 0, "expected forked timelines, got 0");
    assert!(exp.bugs_found > 0, "exploration should have found the bug");
}

/// Replay test: reproduce a bug found by exploration using a fixed seed.
///
/// Steps:
/// 1. Run exploration with a debug seed to find the bug
/// 2. Re-run with the same seed and no exploration to verify deterministic replay
#[test]
fn slow_simulation_control_plane_replay() {
    // Phase 1: Run exploration to find the bug
    let report = run_simulation(
        SimulationBuilder::new()
            .workload(BrokerWorkload::new())
            .workload(ControlPlaneWorkload::new())
            .workload(NodeWorkload::new())
            .workload(DriverWorkload::new())
            .random_network()
            .set_iterations(1)
            .set_debug_seeds(vec![12345])
            .enable_exploration(ExplorationConfig {
                max_depth: 60,
                timelines_per_split: 4,
                global_energy: 100_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 20,
                    min_timelines: 200,
                    max_timelines: 500,
                    per_mark_energy: 5_000,
                    warm_min_timelines: Some(20),
                }),
                parallelism: Some(moonpool_sim::Parallelism::HalfCores),
            }),
    );

    assert_eq!(report.successful_runs, 1);
    let exp = report.exploration.expect("exploration report missing");
    assert!(exp.bugs_found > 0, "exploration should have found the bug");

    // Phase 2: Replay with same seed, no exploration
    let replay_report = run_simulation(
        SimulationBuilder::new()
            .workload(BrokerWorkload::new())
            .workload(ControlPlaneWorkload::new())
            .workload(NodeWorkload::new())
            .workload(DriverWorkload::new())
            .set_iterations(1)
            .set_debug_seeds(vec![12345]),
    );

    assert_eq!(replay_report.successful_runs, 1);
}
