//! Simulation tests for moonpool messaging layer.
//!
//! Tests EndpointMap, FlowTransport, and NetNotifiedQueue under chaos conditions
//! using the FoundationDB-inspired simulation framework.

use std::time::Duration;

use moonpool_foundation::{IterationControl, SimulationBuilder, SimulationReport};

use super::operations::ClientOpWeights;
use super::workloads::{
    LocalDeliveryConfig, RpcWorkloadConfig, endpoint_lifecycle_workload, local_delivery_workload,
    rpc_workload,
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
            .register_workload("node", |random, network, time, task, topology| {
                local_delivery_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    LocalDeliveryConfig {
                        num_operations: 50,
                        weights: ClientOpWeights::default(),
                        receive_timeout: Duration::from_secs(5),
                    },
                )
            })
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
            .register_workload("node", |random, network, time, task, topology| {
                local_delivery_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    LocalDeliveryConfig {
                        num_operations: 100,
                        weights: ClientOpWeights::reliable_focused(),
                        receive_timeout: Duration::from_secs(5),
                    },
                )
            })
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
            .register_workload("node", |random, network, time, task, topology| {
                local_delivery_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    LocalDeliveryConfig {
                        num_operations: 100,
                        weights: ClientOpWeights::unreliable_focused(),
                        receive_timeout: Duration::from_secs(5),
                    },
                )
            })
            .set_iterations(10),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test endpoint lifecycle (register/unregister).
#[test]
fn test_endpoint_lifecycle() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("node", endpoint_lifecycle_workload)
            .set_iterations(10),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Slow Simulation Tests (Chaos Testing)
// ============================================================================

/// Chaos test for local delivery - runs until all sometimes_assert! reached.
#[test]
fn slow_simulation_local_delivery() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .use_random_config() // Enable chaos
            .register_workload("node", |random, network, time, task, topology| {
                local_delivery_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    LocalDeliveryConfig {
                        num_operations: 200,
                        weights: ClientOpWeights::default(),
                        receive_timeout: Duration::from_secs(30),
                    },
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for endpoint lifecycle.
#[test]
fn slow_simulation_endpoint_lifecycle() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .use_random_config() // Enable chaos
            .register_workload("node", endpoint_lifecycle_workload)
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// RPC Tests (Phase 12B)
// ============================================================================

/// Test RPC basic request-response without chaos - verifies happy path.
#[test]
fn test_rpc_basic_request_response() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("node", |random, network, time, task, topology| {
                rpc_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    RpcWorkloadConfig::happy_path(),
                )
            })
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test RPC with broken promises - server drops some promises.
#[test]
fn test_rpc_broken_promise() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("node", |random, network, time, task, topology| {
                rpc_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    RpcWorkloadConfig::broken_promise_focused(),
                )
            })
            .set_iterations(10),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for RPC happy path - runs until all sometimes_assert! reached.
#[test]
fn slow_simulation_rpc_happy_path() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .use_random_config() // Enable chaos
            .register_workload("node", |random, network, time, task, topology| {
                rpc_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    RpcWorkloadConfig::happy_path(),
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for RPC error paths - focuses on broken promises and edge cases.
#[test]
fn slow_simulation_rpc_error_paths() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .use_random_config() // Enable chaos
            .register_workload("node", |random, network, time, task, topology| {
                rpc_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    RpcWorkloadConfig::broken_promise_focused(),
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Debug Test
// ============================================================================

/// Debug a specific failing seed.
#[test]
#[ignore] // Run with `cargo test -- --ignored` when debugging
fn debug_specific_seed() {
    let failing_seed = 42; // Replace with actual failing seed

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .set_debug_seeds(vec![failing_seed])
            .set_iteration_control(IterationControl::FixedCount(1))
            .register_workload("node", |random, network, time, task, topology| {
                local_delivery_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    LocalDeliveryConfig::default(),
                )
            }),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}
