//! Binary target for transport messaging simulation scenarios.
//!
//! Runs all slow messaging simulation scenarios sequentially.
//! Exit code 1 on any failure.

use std::process;

use moonpool_sim::{IterationControl, SimulationBuilder, SimulationReport};
use moonpool_transport::simulations::messaging::operations::ClientOpWeights;
use moonpool_transport::simulations::messaging::workloads::{
    LocalDeliveryConfig, LocalDeliveryWorkload, MultiMethodWorkload, MultiNodeClientConfig,
    MultiNodeClientWorkload, MultiNodeServerConfig, MultiNodeServerWorkload, RpcWorkload,
    RpcWorkloadConfig,
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
        .with_max_level(tracing::Level::WARN)
        .try_init();

    // --- Local delivery ---
    eprintln!("=== Local Delivery ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig {
                num_operations: 200,
                weights: ClientOpWeights::default(),
                receive_timeout: std::time::Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("local_delivery", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- RPC happy path ---
    eprintln!("=== RPC Happy Path ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(RpcWorkload::new(RpcWorkloadConfig::happy_path()))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("rpc_happy_path", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- RPC error paths ---
    eprintln!("=== RPC Error Paths ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(RpcWorkload::new(RpcWorkloadConfig::broken_promise_focused()))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("rpc_error_paths", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Multi-node RPC ---
    eprintln!("=== Multi-Node RPC ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(MultiNodeServerWorkload::new(
                MultiNodeServerConfig::default(),
            ))
            .workload(MultiNodeClientWorkload::new(
                MultiNodeClientConfig::default(),
            ))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("multi_node_rpc", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Multi-method ---
    eprintln!("=== Multi-Method ===");
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(MultiMethodWorkload::new(100))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    eprintln!("{}", report);
    if let Err(e) = check_report("multi_method", &report) {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    eprintln!("All transport messaging scenarios passed.");
}
