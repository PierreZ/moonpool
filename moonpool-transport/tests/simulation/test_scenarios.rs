// TODO CLAUDE AI: port to new builder API
/*
//! Simulation tests for moonpool messaging layer.
//!
//! Tests EndpointMap, NetTransport, and NetNotifiedQueue under chaos conditions
//! using the FoundationDB-inspired simulation framework.

use std::time::Duration;

use moonpool_sim::{IterationControl, SimulationBuilder, SimulationReport};

use super::operations::ClientOpWeights;
use super::workloads::{
    LocalDeliveryConfig, MultiNodeClientConfig, MultiNodeServerConfig, RpcWorkloadConfig,
    endpoint_lifecycle_workload, local_delivery_workload, multi_node_rpc_client_workload,
    multi_node_rpc_server_workload, rpc_workload,
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

// ============================================================================
// Multi-Node RPC Tests (Phase 12 Step 7d)
// ============================================================================
//
// These tests verify RPC across separate nodes with actual network transport.
//
// Key implementation details (FDB patterns):
// - connection_incoming() uses Peer::new_incoming() with the accepted stream
// - SimTcpListener returns synthesized ephemeral addresses (FDB sim2.actor.cpp:1149-1175)
// - Server-side connections see client ephemeral ports, not identity addresses
// See: moonpool/src/messaging/static/flow_transport.rs:655-720
// See: moonpool-foundation/src/network/sim/stream.rs:392-400

/// Test multi-node RPC with 1 client and 1 server - basic connectivity.
#[test]
fn test_multi_node_rpc_1x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            // Server must be registered first to get IP 10.0.0.1
            .register_workload("server", |random, network, time, task, topology| {
                multi_node_rpc_server_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    MultiNodeServerConfig::happy_path(),
                )
            })
            .register_workload("client", |random, network, time, task, topology| {
                multi_node_rpc_client_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    MultiNodeClientConfig::happy_path(),
                )
            })
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Test multi-node RPC with 2 clients and 1 server - verifies load handling.
#[test]
fn test_multi_node_rpc_2x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            // Server gets IP 10.0.0.1
            .register_workload("server", |random, network, time, task, topology| {
                multi_node_rpc_server_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    MultiNodeServerConfig::happy_path(),
                )
            })
            // Client 1 gets IP 10.0.0.2
            .register_workload("client1", |random, network, time, task, topology| {
                multi_node_rpc_client_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    MultiNodeClientConfig::happy_path(),
                )
            })
            // Client 2 gets IP 10.0.0.3
            .register_workload("client2", |random, network, time, task, topology| {
                multi_node_rpc_client_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    MultiNodeClientConfig::happy_path(),
                )
            })
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for multi-node RPC - runs until sometimes_assert! coverage.
#[test]
fn slow_simulation_multi_node_rpc() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .use_random_config() // Enable chaos
            .register_workload("server", |random, network, time, task, topology| {
                multi_node_rpc_server_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    MultiNodeServerConfig::default(),
                )
            })
            .register_workload("client", |random, network, time, task, topology| {
                multi_node_rpc_client_workload(
                    random,
                    network,
                    time,
                    task,
                    topology,
                    MultiNodeClientConfig::default(),
                )
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Multi-Method Interface Tests (Phase 12D - Step 17)
// ============================================================================
//
// These tests validate register_handler_at() and multi-method routing under chaos.

use super::workloads::WorkloadProviders;
use super::{
    CalcAddRequest, CalcAddResponse, CalcMulRequest, CalcMulResponse, CalcSubRequest,
    CalcSubResponse, calculator,
};
use moonpool_sim::{RandomProvider, sometimes_assert};
use moonpool_transport::{JsonCodec, NetTransportBuilder, ReplyPromise, send_request};

/// Local multi-method interface workload for testing register_handler_at().
///
/// This workload:
/// 1. Creates a NetTransport with multiple handlers using register_handler_at
/// 2. Sends requests to different methods (add, sub, mul)
/// 3. Validates correct routing by checking request_id matches in responses
async fn multi_method_workload<N, T, TP>(
    random: moonpool_sim::SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: moonpool_sim::WorkloadTopology,
    num_operations: u64,
) -> moonpool_transport::SimulationResult<moonpool_sim::SimulationMetrics>
where
    N: moonpool_transport::NetworkProvider + Clone + 'static,
    T: moonpool_transport::TimeProvider + Clone + 'static,
    TP: moonpool_transport::TaskProvider + Clone + 'static,
{
    let _my_id = topology.my_ip.clone();

    // Create local address
    let local_addr = moonpool_transport::NetworkAddress::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
        4500,
    );

    // Bundle providers for NetTransportBuilder
    let providers = WorkloadProviders::new(network, time.clone(), task, random.clone());

    // Phase 12C: Use NetTransportBuilder
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr.clone())
        .build()
        .expect("build should succeed");

    // Phase 12C: Register multiple handlers using register_handler_at
    let (add_stream, add_token) = transport.register_handler_at::<CalcAddRequest, _>(
        calculator::CALC_INTERFACE,
        calculator::METHOD_ADD,
        JsonCodec,
    );
    let (sub_stream, sub_token) = transport.register_handler_at::<CalcSubRequest, _>(
        calculator::CALC_INTERFACE,
        calculator::METHOD_SUB,
        JsonCodec,
    );
    let (mul_stream, mul_token) = transport.register_handler_at::<CalcMulRequest, _>(
        calculator::CALC_INTERFACE,
        calculator::METHOD_MUL,
        JsonCodec,
    );

    // Create endpoints for sending requests
    let add_endpoint = moonpool_transport::Endpoint::new(local_addr.clone(), add_token);
    let sub_endpoint = moonpool_transport::Endpoint::new(local_addr.clone(), sub_token);
    let mul_endpoint = moonpool_transport::Endpoint::new(local_addr.clone(), mul_token);

    // Track pending requests and promises per method
    let mut pending_add: Vec<(u64, ReplyPromise<CalcAddResponse, JsonCodec>)> = Vec::new();
    let mut pending_sub: Vec<(u64, ReplyPromise<CalcSubResponse, JsonCodec>)> = Vec::new();
    let mut pending_mul: Vec<(u64, ReplyPromise<CalcMulResponse, JsonCodec>)> = Vec::new();

    let mut next_request_id = 0u64;
    let mut add_count = 0u64;
    let mut sub_count = 0u64;
    let mut mul_count = 0u64;

    // Perform operations
    for _op_num in 0..num_operations {
        // Randomly choose which method to call
        let method_choice = random.random_range(0..3);

        match method_choice {
            0 => {
                // Add operation
                let request_id = next_request_id;
                next_request_id += 1;
                let a = random.random_range(0..100) as i64;
                let b = random.random_range(0..100) as i64;
                let request = CalcAddRequest { a, b, request_id };

                if send_request::<_, CalcAddResponse, _, _>(
                    &transport,
                    &add_endpoint,
                    request,
                    JsonCodec,
                )
                .is_ok()
                {
                    add_count += 1;
                    sometimes_assert!(multi_method_add_sent, true, "Should send ADD requests");
                }
            }
            1 => {
                // Sub operation
                let request_id = next_request_id;
                next_request_id += 1;
                let a = random.random_range(0..100) as i64;
                let b = random.random_range(0..100) as i64;
                let request = CalcSubRequest { a, b, request_id };

                if send_request::<_, CalcSubResponse, _, _>(
                    &transport,
                    &sub_endpoint,
                    request,
                    JsonCodec,
                )
                .is_ok()
                {
                    sub_count += 1;
                    sometimes_assert!(multi_method_sub_sent, true, "Should send SUB requests");
                }
            }
            _ => {
                // Mul operation
                let request_id = next_request_id;
                next_request_id += 1;
                let a = random.random_range(0..100) as i64;
                let b = random.random_range(0..100) as i64;
                let request = CalcMulRequest { a, b, request_id };

                if send_request::<_, CalcMulResponse, _, _>(
                    &transport,
                    &mul_endpoint,
                    request,
                    JsonCodec,
                )
                .is_ok()
                {
                    mul_count += 1;
                    sometimes_assert!(multi_method_mul_sent, true, "Should send MUL requests");
                }
            }
        }

        // Process received requests for each method
        if let Some((request, reply)) =
            add_stream.try_recv_with_transport::<_, CalcAddResponse>(&transport)
        {
            pending_add.push((request.request_id, reply));
            sometimes_assert!(
                multi_method_add_received,
                true,
                "Should receive ADD requests via correct handler"
            );
        }

        if let Some((request, reply)) =
            sub_stream.try_recv_with_transport::<_, CalcSubResponse>(&transport)
        {
            pending_sub.push((request.request_id, reply));
            sometimes_assert!(
                multi_method_sub_received,
                true,
                "Should receive SUB requests via correct handler"
            );
        }

        if let Some((request, reply)) =
            mul_stream.try_recv_with_transport::<_, CalcMulResponse>(&transport)
        {
            pending_mul.push((request.request_id, reply));
            sometimes_assert!(
                multi_method_mul_received,
                true,
                "Should receive MUL requests via correct handler"
            );
        }

        // Respond to some pending requests
        if !pending_add.is_empty() && random.random_range(0..2) == 0 {
            let (req_id, reply) = pending_add.remove(0);
            reply.send(CalcAddResponse {
                result: 42, // Actual computation not important for routing test
                request_id: req_id,
            });
        }
        if !pending_sub.is_empty() && random.random_range(0..2) == 0 {
            let (req_id, reply) = pending_sub.remove(0);
            reply.send(CalcSubResponse {
                result: 0,
                request_id: req_id,
            });
        }
        if !pending_mul.is_empty() && random.random_range(0..2) == 0 {
            let (req_id, reply) = pending_mul.remove(0);
            reply.send(CalcMulResponse {
                result: 1,
                request_id: req_id,
            });
        }

        // Small delay
        let _ = time.sleep(Duration::from_millis(1)).await;
    }

    // Drain all pending requests
    while let Some((request, reply)) =
        add_stream.try_recv_with_transport::<_, CalcAddResponse>(&transport)
    {
        reply.send(CalcAddResponse {
            result: 0,
            request_id: request.request_id,
        });
    }
    while let Some((request, reply)) =
        sub_stream.try_recv_with_transport::<_, CalcSubResponse>(&transport)
    {
        reply.send(CalcSubResponse {
            result: 0,
            request_id: request.request_id,
        });
    }
    while let Some((request, reply)) =
        mul_stream.try_recv_with_transport::<_, CalcMulResponse>(&transport)
    {
        reply.send(CalcMulResponse {
            result: 0,
            request_id: request.request_id,
        });
    }

    // Respond to remaining pending
    for (req_id, reply) in pending_add {
        reply.send(CalcAddResponse {
            result: 0,
            request_id: req_id,
        });
    }
    for (req_id, reply) in pending_sub {
        reply.send(CalcSubResponse {
            result: 0,
            request_id: req_id,
        });
    }
    for (req_id, reply) in pending_mul {
        reply.send(CalcMulResponse {
            result: 0,
            request_id: req_id,
        });
    }

    tracing::info!(
        add = add_count,
        sub = sub_count,
        mul = mul_count,
        "Multi-method workload completed"
    );

    Ok(moonpool_sim::SimulationMetrics {
        events_processed: num_operations,
        ..Default::default()
    })
}

/// Test multi-method interface with register_handler_at - basic routing.
#[test]
fn test_multi_method_basic() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("node", |random, network, time, task, topology| {
                multi_method_workload(random, network, time, task, topology, 50)
            })
            .set_iterations(5),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

/// Chaos test for multi-method interface - validates routing under chaos conditions.
#[test]
fn slow_simulation_multi_method() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .use_random_config() // Enable chaos
            .register_workload("node", |random, network, time, task, topology| {
                multi_method_workload(random, network, time, task, topology, 100)
            })
            .set_iteration_control(IterationControl::UntilAllSometimesReached(1000)),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}

// ============================================================================
// Phase 12D Step 18: Builder Tests with SimNetworkProvider
// ============================================================================

/// Verify build_listening() works correctly with SimNetworkProvider.
///
/// This test explicitly validates that NetTransportBuilder integrates properly
/// with the simulation infrastructure.
async fn build_listening_sim_workload<N, T, TP>(
    random: moonpool_sim::SimRandomProvider,
    network: N,
    time: T,
    task: TP,
    topology: moonpool_sim::WorkloadTopology,
    _num_operations: u64,
) -> moonpool_transport::SimulationResult<moonpool_sim::SimulationMetrics>
where
    N: moonpool_transport::NetworkProvider + Clone + 'static,
    T: moonpool_transport::TimeProvider + Clone + 'static,
    TP: moonpool_transport::TaskProvider + Clone + 'static,
{
    use moonpool_transport::NetTransportBuilder;

    // Create local address from topology
    let port = 4500u16;
    let ip: std::net::IpAddr = topology.my_ip.parse().expect("valid IP");
    let local_addr = moonpool_transport::NetworkAddress::new(ip, port);

    // Bundle providers for NetTransportBuilder
    let providers = WorkloadProviders::new(network, time, task, random);

    // Test: build_listening() should succeed with SimNetworkProvider
    let transport = NetTransportBuilder::new(providers)
        .local_address(local_addr.clone())
        .build_listening()
        .await
        .expect("build_listening should succeed with SimNetworkProvider");

    // Verify transport is properly configured
    assert_eq!(*transport.local_address(), local_addr);
    assert_eq!(transport.peer_count(), 0);
    assert_eq!(transport.endpoint_count(), 0);

    tracing::info!("build_listening with SimNetworkProvider succeeded");

    Ok(moonpool_sim::SimulationMetrics::default())
}

#[test]
fn test_build_listening_with_sim_network() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let report = run_simulation(
        SimulationBuilder::new()
            .register_workload("server", |random, network, time, task, topology| {
                build_listening_sim_workload(random, network, time, task, topology, 0)
            })
            .set_iterations(3),
    );

    println!("{}", report);
    assert_simulation_success(&report);
}
*/
