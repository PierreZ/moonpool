//! Test scenarios for transport simulation testing.
//!
//! Fast tests use fixed seeds for deterministic debugging.
//! Slow tests use many iterations with random network for comprehensive chaos coverage.

use moonpool_sim::SimulationBuilder;

use super::alphabet::AlphabetWeights;
use super::workloads::{ClientWorkload, LocalDeliveryWorkload, ServerWorkload};

/// Helper: assert a report has no failed runs.
fn assert_no_failures(report: &moonpool_sim::SimulationReport) {
    assert_eq!(
        report.failed_runs, 0,
        "Failed seeds: {:?}",
        report.seeds_failing
    );
}

// ============================================================================
// Fast tests (fixed seeds, no assertion contract validation)
// ============================================================================

#[test]
fn test_local_delivery_happy_path() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(LocalDeliveryWorkload::with_weights(
                AlphabetWeights::happy_path(),
            ))
            .set_iterations(3)
            .run()
            .await
    });

    assert_no_failures(&report);
}

#[test]
fn test_local_delivery_adversarial() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(
                LocalDeliveryWorkload::with_weights(AlphabetWeights::adversarial_heavy())
                    .with_ops_count(30),
            )
            .set_iterations(3)
            .run()
            .await
    });

    assert_no_failures(&report);
}

#[test]
fn test_multi_node_rpc_1x1() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(ServerWorkload::new())
            .workload(ClientWorkload::with_config(
                AlphabetWeights::happy_path(),
                30,
            ))
            .set_iterations(3)
            .run()
            .await
    });

    assert_no_failures(&report);
}

#[test]
fn test_multi_node_rpc_with_broken_promises() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(ServerWorkload::with_drop_rate(30))
            .workload(ClientWorkload::with_config(
                AlphabetWeights::happy_path(),
                30,
            ))
            .set_iterations(3)
            .run()
            .await
    });

    assert_no_failures(&report);
}

// ============================================================================
// Slow chaos tests (many iterations with random network)
// ============================================================================

#[test]
fn slow_simulation_local_delivery() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(LocalDeliveryWorkload::new())
            .set_iterations(50)
            .random_network()
            .run()
            .await
    });

    assert_no_failures(&report);
}

#[test]
fn slow_simulation_multi_node_rpc() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(ServerWorkload::with_drop_rate(20))
            .workload(ClientWorkload::new())
            .set_iterations(50)
            .random_network()
            .run()
            .await
    });

    assert_no_failures(&report);
}
