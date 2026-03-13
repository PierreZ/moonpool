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
    builder.run()
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    // --- Reliable delivery ---
    eprintln!("=== Reliable Delivery ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .processes(1, || Box::new(WireServerWorkload::new()))
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::reliable_focused(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    report.eprint();
    if let Err(e) = report.check("reliable_delivery") {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Unreliable drops ---
    eprintln!("=== Unreliable Drops ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .processes(1, || Box::new(WireServerWorkload::new()))
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::unreliable_focused(),
                drain_phase: false,
                drain_timeout: Duration::from_secs(5),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    report.eprint();
    if let Err(e) = report.check("unreliable_drops") {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Mixed queues ---
    eprintln!("=== Mixed Queues ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .processes(1, || Box::new(WireServerWorkload::new()))
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::mixed_queues(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    report.eprint();
    if let Err(e) = report.check("mixed_queues") {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Reconnection ---
    eprintln!("=== Reconnection ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .processes(1, || Box::new(WireServerWorkload::new()))
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::reconnection_focused(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    report.eprint();
    if let Err(e) = report.check("reconnection") {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Multi-client ---
    eprintln!("=== Multi Client ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .processes(1, || Box::new(WireServerWorkload::new()))
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
    report.eprint();
    if let Err(e) = report.check("multi_client") {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    eprintln!("All transport E2E scenarios passed.");
}
