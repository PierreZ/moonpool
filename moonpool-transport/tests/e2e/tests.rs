//! E2E simulation tests for Peer correctness.
//!
//! These tests use SimulationBuilder to run workloads under chaos
//! conditions, validating that Peer behaves correctly.

use moonpool_sim::{IterationControl, SimulationBuilder, SimulationReport};

use super::operations::OpWeights;
use super::workloads::{
    ClientConfig, client_workload, client_workload_with_config, wire_server_workload,
};

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
}

/// Helper to panic if the simulation had failures (strict: also checks assertions).
#[allow(dead_code)]
fn assert_simulation_success_strict(report: &SimulationReport) {
    assert_simulation_success(report);

    if !report.assertion_violations.is_empty() {
        panic!(
            "Simulation had {} assertion violations: {:?}",
            report.assertion_violations.len(),
            report.assertion_violations
        );
    }
}

/// Basic test: Client sends messages to wire-protocol-aware server.
///
/// Validates basic connectivity and message passing with proper wire format parsing.
#[test]
fn test_basic_client_server() {
    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("server", wire_server_workload)
            .register_workload("client", |random, network, time, task, topology| {
                client_workload_with_config(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    ClientConfig {
                        num_operations: 50,
                        weights: OpWeights::reliable_focused(),
                        drain_phase: true,
                        drain_timeout: std::time::Duration::from_secs(5),
                    },
                )
            })
            .set_iterations(3)
            .set_debug_seeds(vec![1, 2, 3]),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test reliable delivery under chaos.
///
/// Uses `UntilAllSometimesReached` to ensure error paths are exercised.
#[test]
fn slow_simulation_reliable_delivery() {
    let report = run_simulation(
        SimulationBuilder::new()
            .use_random_config()
            .register_workload("server", wire_server_workload)
            .register_workload("client", |random, network, time, task, topology| {
                client_workload_with_config(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    ClientConfig {
                        num_operations: 200,
                        weights: OpWeights::reliable_focused(),
                        drain_phase: true,
                        drain_timeout: std::time::Duration::from_secs(30),
                    },
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
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
            .use_random_config()
            .register_workload("server", wire_server_workload)
            .register_workload("client", |random, network, time, task, topology| {
                client_workload_with_config(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    ClientConfig {
                        num_operations: 200,
                        weights: OpWeights::unreliable_focused(),
                        drain_phase: false, // No drain for unreliable
                        drain_timeout: std::time::Duration::from_secs(5),
                    },
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
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
            .use_random_config()
            .register_workload("server", wire_server_workload)
            .register_workload("client", |random, network, time, task, topology| {
                client_workload_with_config(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    ClientConfig {
                        num_operations: 200,
                        weights: OpWeights::mixed_queues(),
                        drain_phase: true,
                        drain_timeout: std::time::Duration::from_secs(30),
                    },
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
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
            .use_random_config()
            .register_workload("server", wire_server_workload)
            .register_workload("client", |random, network, time, task, topology| {
                client_workload_with_config(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    ClientConfig {
                        num_operations: 200,
                        weights: OpWeights::reconnection_focused(),
                        drain_phase: true,
                        drain_timeout: std::time::Duration::from_secs(30),
                    },
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
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
            .use_random_config()
            .register_workload("server", wire_server_workload)
            .register_workload("client_1", |random, network, time, task, topology| {
                client_workload_with_config(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    ClientConfig {
                        num_operations: 100,
                        weights: OpWeights::default(),
                        drain_phase: true,
                        drain_timeout: std::time::Duration::from_secs(30),
                    },
                )
            })
            .register_workload("client_2", |random, network, time, task, topology| {
                client_workload_with_config(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    ClientConfig {
                        num_operations: 100,
                        weights: OpWeights::default(),
                        drain_phase: true,
                        drain_timeout: std::time::Duration::from_secs(30),
                    },
                )
            })
            .register_workload("client_3", |random, network, time, task, topology| {
                client_workload_with_config(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    ClientConfig {
                        num_operations: 100,
                        weights: OpWeights::default(),
                        drain_phase: true,
                        drain_timeout: std::time::Duration::from_secs(30),
                    },
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Debug test for specific seeds.
///
/// Use this to reproduce issues found in chaos testing.
#[test]
#[ignore] // Run with `cargo test -- --ignored` when debugging
fn debug_specific_seed() {
    // Replace with failing seed from chaos testing
    let failing_seed = 42;

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .set_debug_seeds(vec![failing_seed])
            .set_iteration_control(IterationControl::FixedCount(1))
            .register_workload("server", wire_server_workload)
            .register_workload("client", client_workload),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}
