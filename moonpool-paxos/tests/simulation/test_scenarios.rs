//! Simulation test scenarios for Vertical Paxos II.
//!
//! ## Test Organization
//!
//! - `test_paxos_*`: Fast tests without chaos (1-5 iterations, no random network)
//! - `slow_simulation_paxos_*`: Chaos tests (many iterations, random network config)
//!
//! ## Architecture
//!
//! Each test sets up a 5-node topology:
//! - 3 acceptors (workload names: "acceptor_0", "acceptor_1", "acceptor_2")
//! - 1 leader (workload name: "leader", colocated as acceptor_0 in the topology)
//! - 1 client (workload name: "client")
//!
//! The leader is designated as the first acceptor address. In VP II, the master
//! picks the leader; here, the initial bootstrap config uses acceptor_0.

use std::time::Duration;

use moonpool_sim::{IterationControl, SimulationBuilder, SimulationReport};

use super::invariants::{PaxosLivenessChecker, PaxosSafetyChecker};
use super::workloads::{
    AcceptorConfig, AcceptorWorkload, ClientConfig, ClientWorkload, LeaderConfig, LeaderWorkload,
    SharedClusterState, make_initial_config,
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

/// Build a simulation with a standard 5-node Paxos cluster.
///
/// Topology:
/// - Workload 0 ("acceptor_0") at 10.0.0.1 — also the initial leader
/// - Workload 1 ("acceptor_1") at 10.0.0.2
/// - Workload 2 ("acceptor_2") at 10.0.0.3
/// - Workload 3 ("leader") at 10.0.0.1 — shares IP with acceptor_0
/// - Workload 4 ("client") at 10.0.0.5
///
/// Note: The leader workload shares the same IP as acceptor_0 because in VP II,
/// the leader IS one of the acceptors. However, in our simulation, workloads
/// get unique IPs from the SimulationBuilder (10.0.0.N). So the leader workload
/// gets its own IP and the config is built accordingly.
fn build_paxos_simulation(
    acceptor_config: AcceptorConfig,
    leader_config: LeaderConfig,
    client_config: ClientConfig,
) -> SimulationBuilder {
    // The SimulationBuilder assigns IPs based on workload order:
    // workload 0 → 10.0.0.1, workload 1 → 10.0.0.2, etc.
    //
    // We set up:
    //   0: acceptor_0 → 10.0.0.1
    //   1: acceptor_1 → 10.0.0.2
    //   2: acceptor_2 → 10.0.0.3
    //   3: leader     → 10.0.0.4  (separate node for the leader role)
    //   4: client     → 10.0.0.5
    //
    // The acceptors are at 10.0.0.1, 10.0.0.2, 10.0.0.3.
    // The leader is at 10.0.0.4 but the config says the leader address is
    // one of the acceptor addresses — so the leader runs on a different IP
    // but the config points to itself... This is tricky.
    //
    // Actually, let's simplify: the leader IS acceptor_0. So we don't need
    // a separate leader workload — the leader workload replaces acceptor_0.
    // We use 4 workloads:
    //   0: leader (which is also acceptor_0) → 10.0.0.1
    //   1: acceptor_1 → 10.0.0.2
    //   2: acceptor_2 → 10.0.0.3
    //   3: client     → 10.0.0.4

    let acceptor_ips = vec![
        "10.0.0.1".to_string(), // leader (also acceptor_0)
        "10.0.0.2".to_string(), // acceptor_1
        "10.0.0.3".to_string(), // acceptor_2
    ];

    let initial_config = make_initial_config(&acceptor_ips);
    let shared = SharedClusterState::new(initial_config);

    SimulationBuilder::new()
        // Workload 0: Leader (also acts as acceptor_0) → 10.0.0.1
        .workload(LeaderWorkload::new(leader_config, shared.clone()))
        // Workload 1: Acceptor 1 → 10.0.0.2
        .workload(AcceptorWorkload::new("acceptor_1", acceptor_config.clone()))
        // Workload 2: Acceptor 2 → 10.0.0.3
        .workload(AcceptorWorkload::new("acceptor_2", acceptor_config))
        // Workload 3: Client → 10.0.0.4
        .workload(ClientWorkload::new(client_config, shared))
        // Safety invariant
        .invariant(PaxosSafetyChecker)
        // Liveness invariant
        .invariant(PaxosLivenessChecker)
}

// ============================================================================
// Basic Tests (no chaos, few iterations)
// ============================================================================

/// Test basic Paxos consensus — leader activates and serves a few commands.
///
/// This is the simplest possible test: bootstrap a cluster, activate the leader,
/// submit a small number of commands, verify they commit.
#[test]
fn test_paxos_basic_consensus() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        build_paxos_simulation(
            AcceptorConfig::default(),
            LeaderConfig {
                num_commands_to_serve: 5,
                ..Default::default()
            },
            ClientConfig {
                num_commands: 5,
                inter_command_delay: Duration::from_millis(50),
                ..Default::default()
            },
        )
        .set_iterations(3),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test that the leader can serve more commands.
#[test]
fn test_paxos_multiple_commands() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        build_paxos_simulation(
            AcceptorConfig::default(),
            LeaderConfig {
                num_commands_to_serve: 10,
                ..Default::default()
            },
            ClientConfig {
                num_commands: 10,
                inter_command_delay: Duration::from_millis(20),
                ..Default::default()
            },
        )
        .set_iterations(3),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Chaos Tests (random network, many iterations)
// ============================================================================

/// Chaos test for basic Paxos consensus.
///
/// Runs with randomized network configuration (delays, connection failures,
/// bit flips) to exercise error paths and timeout handling.
#[test]
fn slow_simulation_paxos_consensus() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        build_paxos_simulation(
            AcceptorConfig {
                run_duration: Duration::from_secs(60),
            },
            LeaderConfig {
                num_commands_to_serve: 10,
                activation_timeout: Duration::from_secs(30),
                serve_duration: Duration::from_secs(45),
            },
            ClientConfig {
                num_commands: 10,
                inter_command_delay: Duration::from_millis(100),
                command_timeout: Duration::from_secs(15),
            },
        )
        .random_network()
        .set_iteration_control(IterationControl::FixedCount(50)),
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
fn debug_paxos_seed() {
    let failing_seed = 42;

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let report = run_simulation(
        build_paxos_simulation(
            AcceptorConfig::default(),
            LeaderConfig::default(),
            ClientConfig::default(),
        )
        .set_debug_seeds(vec![failing_seed])
        .set_iteration_control(IterationControl::FixedCount(1)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}
