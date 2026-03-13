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
    builder.run()
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
    report.eprint();
    if let Err(e) = report.check("local_delivery") {
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
    report.eprint();
    if let Err(e) = report.check("rpc_happy_path") {
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
    report.eprint();
    if let Err(e) = report.check("rpc_error_paths") {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    // --- Multi-node RPC ---
    eprintln!("=== Multi-Node RPC ===");
    let server_config = MultiNodeServerConfig::default();
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .processes(1, move || {
                Box::new(MultiNodeServerWorkload::new(server_config.clone()))
            })
            .workload(MultiNodeClientWorkload::new(
                MultiNodeClientConfig::default(),
            ))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );
    report.eprint();
    if let Err(e) = report.check("multi_node_rpc") {
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
    report.eprint();
    if let Err(e) = report.check("multi_method") {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    eprintln!("All transport messaging scenarios passed.");
}
