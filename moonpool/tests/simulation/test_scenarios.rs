//! Test scenarios for virtual actor simulation testing.
//!
//! Uses SimulationBuilder with BankingWorkload to test the actor system
//! under various conditions.

use moonpool_sim::SimulationBuilder;

use super::alphabet::OpWeights;
use super::workloads::BankingWorkload;

/// Helper: assert a report has no failed runs.
fn assert_no_failures(report: &moonpool_sim::SimulationReport) {
    assert_eq!(
        report.failed_runs, 0,
        "Failed seeds: {:?}",
        report.seeds_failing
    );
}

#[test]
fn test_banking_happy_path() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(BankingWorkload::with_config(
                OpWeights::normal_only(),
                30,
                3,
            ))
            .set_iterations(3)
            .run()
            .await
    });

    assert_no_failures(&report);
}

#[test]
fn test_banking_adversarial() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(BankingWorkload::with_config(
                OpWeights::with_adversarial(),
                40,
                5,
            ))
            .set_iterations(3)
            .run()
            .await
    });

    assert_no_failures(&report);
}

#[test]
fn test_banking_nemesis() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(BankingWorkload::new())
            .set_iterations(3)
            .run()
            .await
    });

    assert_no_failures(&report);
}

#[test]
fn slow_simulation_banking() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(Default::default())
        .expect("runtime");

    let report = runtime.block_on(async {
        SimulationBuilder::new()
            .workload(BankingWorkload::with_config(OpWeights::default(), 80, 8))
            .set_iterations(50)
            .run()
            .await
    });

    assert_no_failures(&report);
}
