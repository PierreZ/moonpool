//! E2E simulation tests for Peer correctness.
//!
//! These tests use SimulationBuilder to run workloads under chaos
//! conditions, validating that Peer behaves correctly.

use std::time::Duration;

use moonpool_sim::{IterationControl, SimulationBuilder, SimulationReport};

use super::operations::OpWeights;
use super::workloads::{ClientConfig, ClientWorkload, WireServerWorkload};

// ============================================================================
// Test Utilities
// ============================================================================

/// Helper to run a simulation and check for failures.
fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

/// Helper to panic if the simulation had failures.
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

/// Basic test: Client sends messages to wire-protocol-aware server.
///
/// Validates basic connectivity and message passing with proper wire format parsing.
#[test]
fn test_basic_client_server() {
    let report = run_simulation(
        SimulationBuilder::new()
            .workload(WireServerWorkload::new())
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 50,
                weights: OpWeights::reliable_focused(),
                drain_phase: true,
                drain_timeout: Duration::from_secs(5),
            }))
            .set_iterations(3)
            .set_debug_seeds(vec![1, 2, 3]),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Slow Simulation Tests (Chaos Testing)
// ============================================================================

/// Test reliable delivery under chaos.
#[test]
fn slow_simulation_reliable_delivery() {
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

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test unreliable message handling.
///
/// Verifies that unreliable messages are correctly dropped on connection failure.
#[test]
fn slow_simulation_unreliable_drops() {
    let report = run_simulation(
        SimulationBuilder::new()
            .random_network()
            .workload(WireServerWorkload::new())
            .workload(ClientWorkload::new(ClientConfig {
                num_operations: 200,
                weights: OpWeights::unreliable_focused(),
                drain_phase: false, // No drain for unreliable
                drain_timeout: Duration::from_secs(5),
            }))
            .set_iteration_control(IterationControl::FixedCount(100)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test mixed reliable/unreliable queues.
///
/// Verifies that reliable messages have priority over unreliable.
#[test]
fn slow_simulation_mixed_queues() {
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

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test reconnection behavior.
///
/// Verifies that reliable messages survive reconnection.
#[test]
fn slow_simulation_reconnection() {
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

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test with multiple clients.
///
/// Verifies that server handles multiple concurrent connections.
#[test]
fn slow_simulation_multi_client() {
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

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Debug Test
// ============================================================================

/// Debug test for specific seeds.
///
/// Use this to reproduce issues found in chaos testing.
#[test]
#[ignore] // Run with `cargo test -- --ignored` when debugging
fn debug_specific_seed() {
    let failing_seed = 42;

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .set_debug_seeds(vec![failing_seed])
            .set_iteration_control(IterationControl::FixedCount(1))
            .workload(WireServerWorkload::new())
            .workload(ClientWorkload::new(ClientConfig::default())),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}
