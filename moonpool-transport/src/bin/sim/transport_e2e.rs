//! Binary target for transport E2E simulation scenarios.
//!
//! Runs all slow E2E simulation scenarios sequentially.
//! Exit code 1 on any failure.

use std::process;
use std::time::Duration;

use moonpool_sim::{IterationControl, SimulationBuilder, SimulationReport};
use moonpool_transport::simulations::e2e::operations::OpWeights;
use moonpool_transport::simulations::e2e::workloads::{
    ClientConfig, ClientWorkload, WireServerWorkload,
};

fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

fn check_report(name: &str, report: &SimulationReport) -> Result<(), String> {
    if !report.seeds_failing.is_empty() {
        return Err(format!(
            "{}: {} failing seeds: {:?}",
            name,
            report.seeds_failing.len(),
            report.seeds_failing
        ));
    }
    if !report.assertion_violations.is_empty() {
        return Err(format!(
            "{}: assertion violations:\n{}",
            name,
            report
                .assertion_violations
                .iter()
                .map(|v| format!("  - {}", v))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    Ok(())
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    // --- Reliable delivery ---
    eprintln!("=== Reliable Delivery ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(WireServerWorkload::new())
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::reliable_focused(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("reliable_delivery", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Unreliable drops ---
    eprintln!("=== Unreliable Drops ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(WireServerWorkload::new())
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::unreliable_focused(),
                drain_phase: false,
                drain_timeout: Duration::from_secs(5),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("unreliable_drops", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Mixed queues ---
    eprintln!("=== Mixed Queues ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(WireServerWorkload::new())
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::mixed_queues(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("mixed_queues", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Reconnection ---
    eprintln!("=== Reconnection ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(WireServerWorkload::new())
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::reconnection_focused(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("reconnection", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Multi-client ---
    eprintln!("=== Multi Client ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(WireServerWorkload::new())
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 100,
                weights: OpWeights::default(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 100,
                weights: OpWeights::default(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 100,
                weights: OpWeights::default(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("multi_client", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    eprintln!("All transport E2E scenarios passed.");
}
