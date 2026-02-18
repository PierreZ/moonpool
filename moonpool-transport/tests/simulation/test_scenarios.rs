//! Simulation tests for moonpool messaging layer.
//!
//! Tests EndpointMap, NetTransport, and NetNotifiedQueue under chaos conditions
//! using the FoundationDB-inspired simulation framework.

use std::time::Duration;

use moonpool_sim::{IterationControl, SimulationBuilder, SimulationReport};

use super::operations::ClientOpWeights;
use super::workloads::{
    LocalDeliveryConfig, LocalDeliveryWorkload, MultiMethodWorkload, MultiNodeClientConfig,
    MultiNodeClientWorkload, MultiNodeServerConfig, MultiNodeServerWorkload, RpcWorkload,
    RpcWorkloadConfig,
};

// ============================================================================
// Test Utilities
// ============================================================================

fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

fn assert_simulation_success(report: &SimulationReport) {
    if !report.seeds_failing.is_empty() {
        panic!(
            "Simulation had {} failing seeds: {:?}",
            report.seeds_failing.len(),
            report.seeds_failing
        );
    }
    if !report.assertion_violations.is_empty() {
        panic!(
            "Assertion violations:\n{}",
            report
                .assertion_violations
                .iter()
                .map(|v| format!("  - {}", v))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

// ============================================================================
// Basic Tests
// ============================================================================

/// Test local delivery without chaos - verifies basic functionality.
#[test]
fn test_local_delivery_basic() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig {
                num_operations: 50,
                weights: ClientOpWeights::default(),
                receive_timeout: Duration::from_secs(5),
            }))
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test local delivery with reliable-focused operations.
#[test]
fn test_local_delivery_reliable() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig {
                num_operations: 100,
                weights: ClientOpWeights::reliable_focused(),
                receive_timeout: Duration::from_secs(5),
            }))
            .set_iterations(10),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test local delivery with unreliable-focused operations.
#[test]
fn test_local_delivery_unreliable() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig {
                num_operations: 100,
                weights: ClientOpWeights::unreliable_focused(),
                receive_timeout: Duration::from_secs(5),
            }))
            .set_iterations(10),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Slow Simulation Tests (Chaos Testing)
// ============================================================================

/// Chaos test for local delivery.
#[test]
fn slow_simulation_local_delivery() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig {
                num_operations: 200,
                weights: ClientOpWeights::default(),
                receive_timeout: Duration::from_secs(30),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// RPC Tests
// ============================================================================

/// Test RPC basic request-response without chaos.
#[test]
fn test_rpc_basic_request_response() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(RpcWorkload::new(RpcWorkloadConfig::happy_path()))
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test RPC with broken promises.
#[test]
fn test_rpc_broken_promise() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(RpcWorkload::new(RpcWorkloadConfig::broken_promise_focused()))
            .set_iterations(10),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for RPC happy path.
#[test]
fn slow_simulation_rpc_happy_path() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(RpcWorkload::new(RpcWorkloadConfig::happy_path()))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for RPC error paths.
#[test]
fn slow_simulation_rpc_error_paths() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(RpcWorkload::new(RpcWorkloadConfig::broken_promise_focused()))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Multi-Node RPC Tests
// ============================================================================

/// Test multi-node RPC with 1 client and 1 server.
#[test]
fn test_multi_node_rpc_1x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(MultiNodeServerWorkload::new(
                MultiNodeServerConfig::happy_path(),
            ))
            .workload(MultiNodeClientWorkload::new(
                MultiNodeClientConfig::happy_path(),
            ))
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test multi-node RPC with 2 clients and 1 server.
#[test]
fn test_multi_node_rpc_2x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(MultiNodeServerWorkload::new(
                MultiNodeServerConfig::happy_path(),
            ))
            .workload(MultiNodeClientWorkload::new(
                MultiNodeClientConfig::happy_path(),
            ))
            .workload(MultiNodeClientWorkload::new(
                MultiNodeClientConfig::happy_path(),
            ))
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for multi-node RPC.
#[test]
fn slow_simulation_multi_node_rpc() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

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

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Multi-Method Interface Tests
// ============================================================================

/// Test multi-method interface basic routing.
#[test]
fn test_multi_method_basic() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(MultiMethodWorkload::new(50))
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for multi-method interface.
#[test]
fn slow_simulation_multi_method() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(MultiMethodWorkload::new(100))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Debug Test
// ============================================================================

/// Debug a specific failing seed.
#[test]
#[ignore]
fn debug_specific_seed() {
    let failing_seed = 42;

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .set_debug_seeds(vec![failing_seed])
            .set_iteration_control(IterationControl::FixedCount(1))
            .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig::default())),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}
