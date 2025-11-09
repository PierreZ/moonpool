//! Autonomous ping-pong test using operation generators.
//!
//! This test demonstrates the power of autonomous testing vs traditional
//! fixed-script testing. Instead of always sending exactly 10 pings in a
//! fixed pattern, this test generates 100-500 diverse operations that explore:
//!
//! - Random operation ordering and timing
//! - Concurrent ping floods and server switching
//! - Timeout scenarios with varied durations
//! - Connection establishment/teardown patterns
//! - Edge cases that manual scripts miss
//!
//! The operation generator approach finds bugs in:
//! - Concurrent disconnect/reconnect scenarios
//! - Server selection during high load
//! - Timeout handling under backpressure
//! - Message correlation edge cases

use std::collections::HashMap;

use moonpool_foundation::{
    SimulationBuilder, SimulationMetrics, SimulationResult, TimeProvider, TokioNetworkProvider,
    TokioRunner, TokioTaskProvider, TokioTimeProvider, WorkloadTopology,
    assertions::panic_on_assertion_violations, buggify_with_prob, network::Envelope,
    random::RandomProvider, runner::IterationControl,
};

/// Operations that a ping-pong client can perform
#[derive(Clone, Debug, PartialEq)]
enum PingPongOp {
    /// Send a ping with specified timeout in milliseconds
    SendPing { timeout_ms: u64 },
    /// Wait for a short period (simulates think time)
    Wait { duration_ms: u64 },
    /// Switch to a different server (if multiple available)
    SwitchServer,
    /// Send burst of pings to fill peer queue (triggered by buggify)
    BurstSend { count: usize, timeout_ms: u64 },
}

/// Generate a ping-pong operation with randomized parameters
fn generate_ping_pong_operation<R: RandomProvider>(random: &R) -> PingPongOp {
    // Use buggify to occasionally generate burst sends (triggers queue growth)
    if buggify_with_prob!(0.05) {
        // 5% chance: Send burst to fill queue
        return PingPongOp::BurstSend {
            count: random.random_range(5..20) as usize,
            timeout_ms: random.random_range(1..50), // Short timeouts to cause queue buildup
        };
    }

    // 70% send ping, 20% wait, 10% switch server
    let choice = random.random_ratio();

    if choice < 0.7 {
        // Send ping with either short or normal timeout
        let timeout_ms = if random.random_bool(0.3) {
            random.random_range(1..10) // Very short: stress test
        } else {
            random.random_range(100..1000) // Normal
        };
        PingPongOp::SendPing { timeout_ms }
    } else if choice < 0.9 {
        // Short wait to vary timing
        PingPongOp::Wait {
            duration_ms: random.random_range(1..50),
        }
    } else {
        // Switch server to test multi-server scenarios
        PingPongOp::SwitchServer
    }
}

/// Autonomous client workload using operation generator
async fn autonomous_ping_pong_client<R, N, T>(
    random: R,
    provider: N,
    time_provider: T,
    task_provider: moonpool_foundation::TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics>
where
    R: RandomProvider,
    N: moonpool_foundation::NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
{
    use moonpool_foundation::{
        PeerConfig,
        network::transport::{ClientTransport, RequestResponseEnvelope},
        sometimes_assert,
    };

    // Use randomized config to create varied test conditions and trigger queue assertions
    let max_queue_size = random.random_range(5..50); // Small queues trigger capacity assertions
    let connection_timeout_ms = random.random_range(100..1000);
    let initial_reconnect_ms = random.random_range(5..50);
    let max_reconnect_ms = random.random_range(500..2000);
    let max_failures = if random.random_range(0..2) == 0 {
        Some(0) // No retries - fills queue faster
    } else {
        Some(random.random_range(1..5) as u32) // Few retries
    };

    let peer_config = PeerConfig::new(
        max_queue_size,
        std::time::Duration::from_millis(connection_timeout_ms),
        std::time::Duration::from_millis(initial_reconnect_ms),
        std::time::Duration::from_millis(max_reconnect_ms),
        max_failures,
    );
    let transport =
        ClientTransport::new(provider, time_provider.clone(), task_provider, peer_config);

    // Get all server addresses
    let server_addresses: Vec<String> = if topology.peer_ips.is_empty() {
        vec!["127.0.0.1:8080".to_string()]
    } else {
        topology.peer_ips.clone()
    };

    let mut current_server_index = 0;
    let mut messages_sent = 0;
    let mut pongs_received = 0;
    let mut timeouts = 0;
    let mut pings_per_peer: HashMap<String, u64> = HashMap::new();
    let mut pongs_per_peer: HashMap<String, u64> = HashMap::new();

    // Register initial state immediately so invariants don't fail on first check
    topology.state_registry.register_state(
        &topology.my_ip,
        serde_json::json!({
            "role": "client",
            "pings_sent": 0,
            "pongs_received": 0,
            "in_transit": 0,
            "pings_per_peer": {},
            "pongs_per_peer": {},
        }),
    );

    // Generate 100-500 operations autonomously
    let num_operations = random.random_range(100..500);

    tracing::trace!(
        "Autonomous client: Generating {} operations",
        num_operations
    );

    for op_num in 0..num_operations {
        // Check shutdown signal
        if topology.shutdown_signal.is_cancelled() {
            tracing::debug!("Client: Shutdown signal received after {} ops", op_num);
            break;
        }

        // Generate next operation
        let op = generate_ping_pong_operation(&random);

        tracing::trace!("Client: Operation {}: {:?}", op_num, op);

        // Helper function to update state incrementally during execution
        let update_state = |sent: u64,
                            received: u64,
                            pings_pp: &HashMap<String, u64>,
                            pongs_pp: &HashMap<String, u64>| {
            topology.state_registry.register_state(
                &topology.my_ip,
                serde_json::json!({
                    "role": "client",
                    "pings_sent": sent,
                    "pongs_received": received,
                    "in_transit": sent.saturating_sub(received),
                    "pings_per_peer": pings_pp,
                    "pongs_per_peer": pongs_pp,
                }),
            );
        };

        match op {
            PingPongOp::SendPing { timeout_ms } => {
                let server = server_addresses[current_server_index].clone();
                messages_sent += 1;
                *pings_per_peer.entry(server.clone()).or_insert(0) += 1;

                // Update state IMMEDIATELY after incrementing counter to avoid race with server
                update_state(
                    messages_sent,
                    pongs_received,
                    &pings_per_peer,
                    &pongs_per_peer,
                );

                tracing::warn!(
                    "CLIENT_SEND: correlation_id={}, messages_sent={}, operation=SendPing, timeout_ms={}",
                    messages_sent,
                    messages_sent,
                    timeout_ms
                );

                sometimes_assert!(
                    autonomous_client_sends_pings,
                    true,
                    "Autonomous client sending pings"
                );

                // Send ping with generated timeout
                match transport
                    .send_with_timeout(
                        &server,
                        RequestResponseEnvelope::new(
                            messages_sent,
                            format!("PING:{}", topology.my_ip).into_bytes(),
                        ),
                        std::time::Duration::from_millis(timeout_ms),
                    )
                    .await
                {
                    Ok(response) => {
                        let response_str = String::from_utf8_lossy(response.payload());
                        if response_str.starts_with("PONG:") {
                            let server_ip = response_str
                                .strip_prefix("PONG:")
                                .unwrap_or("unknown")
                                .to_string();
                            pongs_received += 1;
                            *pongs_per_peer.entry(server_ip).or_insert(0) += 1;
                            update_state(
                                messages_sent,
                                pongs_received,
                                &pings_per_peer,
                                &pongs_per_peer,
                            );

                            sometimes_assert!(
                                autonomous_client_receives_pongs,
                                true,
                                "Autonomous client receiving pongs"
                            );

                            // Assert on short timeouts succeeding (tests backpressure handling)
                            if timeout_ms < 10 {
                                sometimes_assert!(
                                    short_timeout_succeeds,
                                    true,
                                    "Short timeout ping succeeded"
                                );
                            }
                        }
                    }
                    Err(_) => {
                        timeouts += 1;

                        sometimes_assert!(
                            autonomous_client_timeout,
                            true,
                            "Autonomous client experiencing timeout"
                        );
                    }
                }
                // Note: State already updated immediately after incrementing messages_sent
            }

            PingPongOp::Wait { duration_ms } => {
                // Short wait to vary timing and create interesting interleavings
                let _ = time_provider
                    .sleep(std::time::Duration::from_millis(duration_ms))
                    .await;

                sometimes_assert!(
                    autonomous_client_waits,
                    true,
                    "Autonomous client inserting wait"
                );

                // Update state after wait (no change, but keeps state fresh)
                update_state(
                    messages_sent,
                    pongs_received,
                    &pings_per_peer,
                    &pongs_per_peer,
                );
            }

            PingPongOp::SwitchServer => {
                if server_addresses.len() > 1 {
                    let old_index = current_server_index;
                    current_server_index = (current_server_index + 1) % server_addresses.len();

                    sometimes_assert!(
                        autonomous_client_switches_servers,
                        current_server_index != old_index,
                        "Autonomous client switching servers"
                    );
                }

                // Update state after server switch
                update_state(
                    messages_sent,
                    pongs_received,
                    &pings_per_peer,
                    &pongs_per_peer,
                );
            }

            PingPongOp::BurstSend { count, timeout_ms } => {
                // Send burst of messages rapidly to fill peer queue
                let server = server_addresses[current_server_index].clone();

                sometimes_assert!(
                    autonomous_client_sends_burst,
                    true,
                    "Autonomous client sending burst to fill queue"
                );

                // Fire off multiple sends without awaiting (causes queue buildup)
                for _ in 0..count {
                    messages_sent += 1;
                    *pings_per_peer.entry(server.clone()).or_insert(0) += 1;
                    let msg_id = messages_sent;

                    // Update state IMMEDIATELY after incrementing counter to avoid race with server
                    update_state(
                        messages_sent,
                        pongs_received,
                        &pings_per_peer,
                        &pongs_per_peer,
                    );

                    tracing::warn!(
                        "CLIENT_SEND: correlation_id={}, messages_sent={}, operation=BurstSend",
                        msg_id,
                        messages_sent
                    );

                    // Use short timeout for burst sends to maximize queue pressure
                    let _ = transport
                        .send_with_timeout(
                            &server,
                            RequestResponseEnvelope::new(
                                msg_id,
                                format!("PING:{}", topology.my_ip).into_bytes(),
                            ),
                            std::time::Duration::from_millis(timeout_ms),
                        )
                        .await;

                    // Don't await response - just fire and continue
                    // This creates backpressure and fills the peer send queue
                }
                // Note: State already updated in the loop after each send
            }
        }
    }

    tracing::trace!(
        "Autonomous client: Completed {} operations (sent={}, received={}, timeouts={})",
        num_operations,
        messages_sent,
        pongs_received,
        timeouts
    );

    // Update final state for invariant checking
    // Note: State is also updated incrementally during the loop (see below)
    tracing::warn!(
        "CLIENT_STATE_REGISTER_FINAL: ip={}, messages_sent={}, pongs_received={}",
        topology.my_ip,
        messages_sent,
        pongs_received
    );

    topology.state_registry.register_state(
        &topology.my_ip,
        serde_json::json!({
            "role": "client",
            "pings_sent": messages_sent,
            "pongs_received": pongs_received,
            "in_transit": messages_sent.saturating_sub(pongs_received),
            "pings_per_peer": pings_per_peer,
            "pongs_per_peer": pongs_per_peer,
        }),
    );

    Ok(SimulationMetrics::default())
}

/// Simple server that responds to pings
async fn simple_ping_pong_server<R, N, T>(
    _random: R,
    provider: N,
    time_provider: T,
    task_provider: moonpool_foundation::TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics>
where
    R: RandomProvider,
    N: moonpool_foundation::NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
{
    use moonpool_foundation::{
        SimulationError,
        network::transport::{RequestResponseEnvelope, ServerTransport},
        sometimes_assert,
    };

    let mut transport = ServerTransport::<_, _, _, RequestResponseEnvelope>::bind(
        provider,
        time_provider,
        task_provider,
        &topology.my_ip,
    )
    .await
    .map_err(|e| SimulationError::IoError(format!("Server bind failed: {}", e)))?;

    let mut pings_received = 0;
    let mut pongs_sent = 0;
    let mut pings_per_peer: HashMap<String, u64> = HashMap::new();
    let mut pongs_per_peer: HashMap<String, u64> = HashMap::new();

    // Register initial state immediately so invariants don't fail on first check
    topology.state_registry.register_state(
        &topology.my_ip,
        serde_json::json!({
            "role": "server",
            "pings_received": 0,
            "pongs_sent": 0,
            "pings_per_peer": {},
            "pongs_per_peer": {},
            "in_transit_per_peer": {},
        }),
    );

    // Helper function to update state with per-peer tracking
    let update_state = |pings_rcvd: u64,
                        pongs_snt: u64,
                        pings_pp: &HashMap<String, u64>,
                        pongs_pp: &HashMap<String, u64>| {
        // Calculate in_transit_per_peer: pings received but not yet ponged
        let mut in_transit_per_peer = HashMap::new();
        for (peer, pings) in pings_pp {
            let pongs = pongs_pp.get(peer).copied().unwrap_or(0);
            let in_transit = pings.saturating_sub(pongs);
            if in_transit > 0 {
                in_transit_per_peer.insert(peer.clone(), in_transit);
            }
        }

        topology.state_registry.register_state(
            &topology.my_ip,
            serde_json::json!({
                "role": "server",
                "pings_received": pings_rcvd,
                "pongs_sent": pongs_snt,
                "pings_per_peer": pings_pp,
                "pongs_per_peer": pongs_pp,
                "in_transit_per_peer": in_transit_per_peer,
            }),
        );
    };

    loop {
        tokio::select! {
            _ = topology.shutdown_signal.cancelled() => {
                tracing::debug!("Server: Shutdown signal received");
                break;
            }

            Some(msg) = transport.next_message() => {
                let correlation_id = msg.envelope.correlation_id();

                tracing::warn!(
                    "SERVER_RECV: correlation_id={}, pings_received_before={}, connection_id={}",
                    correlation_id,
                    pings_received,
                    msg.connection_id
                );

                // Extract client IP from payload
                let request_str = String::from_utf8_lossy(msg.envelope.payload());
                if request_str.starts_with("PING:") {
                    let client_ip = request_str.strip_prefix("PING:").unwrap_or("unknown").to_string();

                    pings_received += 1;
                    *pings_per_peer.entry(client_ip.clone()).or_insert(0) += 1;
                    update_state(pings_received, pongs_sent, &pings_per_peer, &pongs_per_peer);

                    sometimes_assert!(
                        server_receives_pings,
                        true,
                        "Server receiving pings"
                    );

                    // Respond with PONG
                    let pong_response = format!("PONG:{}", topology.my_ip);
                    match transport.reply_with_payload(
                        &msg.envelope,
                        pong_response.into_bytes(),
                        &msg,
                    ) {
                        Ok(_) => {
                            pongs_sent += 1;
                            *pongs_per_peer.entry(client_ip).or_insert(0) += 1;
                            update_state(pings_received, pongs_sent, &pings_per_peer, &pongs_per_peer);

                            sometimes_assert!(
                                server_sends_pongs,
                                true,
                                "Server sending pongs"
                            );
                        }
                        Err(_) => {
                            // Failed to send, don't increment counter
                        }
                    }
                }
            }
        }
    }

    // Update final state
    tracing::warn!(
        "SERVER_STATE_REGISTER: ip={}, pings_received={}, pongs_sent={}",
        topology.my_ip,
        pings_received,
        pongs_sent
    );

    update_state(pings_received, pongs_sent, &pings_per_peer, &pongs_per_peer);

    Ok(SimulationMetrics::default())
}

/// Message conservation invariant checker
fn create_message_conservation_invariant() -> moonpool_foundation::InvariantCheck {
    Box::new(|states, _time, seed| {
        let mut total_pings_sent = 0u64;
        let mut total_pongs_received = 0u64;
        let mut total_pings_received = 0u64;
        let mut total_pongs_sent = 0u64;

        tracing::warn!(
            "INVARIANT_CHECK: num_states={}, seed={}",
            states.len(),
            seed
        );

        for (key, state) in states.iter() {
            tracing::warn!("INVARIANT_STATE: key={}, state={:?}", key, state);
            if let Some(role) = state.get("role").and_then(|r| r.as_str()) {
                match role {
                    "client" => {
                        total_pings_sent += state
                            .get("pings_sent")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        total_pongs_received += state
                            .get("pongs_received")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                    "server" => {
                        total_pings_received += state
                            .get("pings_received")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        total_pongs_sent += state
                            .get("pongs_sent")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }

        // BUG DETECTOR 1: Servers can't receive more pings than clients sent
        assert!(
            total_pings_received <= total_pings_sent,
            "SEED={}: Message duplication detected: {} pings received by servers > {} pings sent by clients",
            seed,
            total_pings_received,
            total_pings_sent
        );

        // BUG DETECTOR 2: Servers can't send more pongs than pings they received
        assert!(
            total_pongs_sent <= total_pings_received,
            "SEED={}: Server duplication bug: {} pongs sent > {} pings received by servers",
            seed,
            total_pongs_sent,
            total_pings_received
        );

        // BUG DETECTOR 3: Clients can't receive more pongs than servers sent
        assert!(
            total_pongs_received <= total_pongs_sent,
            "SEED={}: Response duplication detected: {} pongs received > {} pongs sent by servers",
            seed,
            total_pongs_received,
            total_pongs_sent
        );

        // BUG DETECTOR 4: Clients can't receive more pongs than pings they sent
        assert!(
            total_pongs_received <= total_pings_sent,
            "SEED={}: Correlation bug: {} pongs received > {} pings sent by clients",
            seed,
            total_pongs_received,
            total_pings_sent
        );

        // BUG DETECTOR 5: Per-peer accounting must be consistent
        for (key, state) in states.iter() {
            if let Some(role) = state.get("role").and_then(|r| r.as_str()) {
                if role == "client" {
                    if let Some(per_peer) = state.get("pings_per_peer").and_then(|p| p.as_object())
                    {
                        let sum_per_peer: u64 = per_peer.values().filter_map(|v| v.as_u64()).sum();
                        let total = state
                            .get("pings_sent")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        assert_eq!(
                            sum_per_peer, total,
                            "SEED={}: Client {} per-peer ping accounting broken: sum={} != total={}",
                            seed, key, sum_per_peer, total
                        );
                    }
                    if let Some(per_peer) = state.get("pongs_per_peer").and_then(|p| p.as_object())
                    {
                        let sum_per_peer: u64 = per_peer.values().filter_map(|v| v.as_u64()).sum();
                        let total = state
                            .get("pongs_received")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        assert_eq!(
                            sum_per_peer, total,
                            "SEED={}: Client {} per-peer pong accounting broken: sum={} != total={}",
                            seed, key, sum_per_peer, total
                        );
                    }
                } else if role == "server" {
                    if let Some(per_peer) = state.get("pings_per_peer").and_then(|p| p.as_object())
                    {
                        let sum_per_peer: u64 = per_peer.values().filter_map(|v| v.as_u64()).sum();
                        let total = state
                            .get("pings_received")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        assert_eq!(
                            sum_per_peer, total,
                            "SEED={}: Server {} per-peer ping accounting broken: sum={} != total={}",
                            seed, key, sum_per_peer, total
                        );
                    }
                    if let Some(per_peer) = state.get("pongs_per_peer").and_then(|p| p.as_object())
                    {
                        let sum_per_peer: u64 = per_peer.values().filter_map(|v| v.as_u64()).sum();
                        let total = state
                            .get("pongs_sent")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        assert_eq!(
                            sum_per_peer, total,
                            "SEED={}: Server {} per-peer pong accounting broken: sum={} != total={}",
                            seed, key, sum_per_peer, total
                        );
                    }
                }
            }
        }

        // BUG DETECTOR 6: Global message conservation with in-transit accounting
        let mut total_client_in_transit = 0u64;
        for state in states.values() {
            if let Some(role) = state.get("role").and_then(|r| r.as_str())
                && role == "client"
            {
                total_client_in_transit += state
                    .get("in_transit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
            }
        }

        assert_eq!(
            total_pongs_received + total_client_in_transit,
            total_pings_sent,
            "SEED={}: Global conservation with client in-transit violated: {} pongs + {} client in-transit != {} pings sent",
            seed,
            total_pongs_received,
            total_client_in_transit,
            total_pings_sent
        );

        // BUG DETECTOR 7: Per-server in-transit consistency
        for (key, state) in states.iter() {
            if let Some(role) = state.get("role").and_then(|r| r.as_str())
                && role == "server"
            {
                let pings_received = state
                    .get("pings_received")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let pongs_sent = state
                    .get("pongs_sent")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let expected_in_transit = pings_received.saturating_sub(pongs_sent);

                if let Some(in_transit_map) =
                    state.get("in_transit_per_peer").and_then(|m| m.as_object())
                {
                    let actual_in_transit: u64 =
                        in_transit_map.values().filter_map(|v| v.as_u64()).sum();
                    assert_eq!(
                        actual_in_transit, expected_in_transit,
                        "SEED={}: Server {} in-transit accounting broken: sum of in_transit_per_peer={} != (pings_received={} - pongs_sent={})",
                        seed, key, actual_in_transit, pings_received, pongs_sent
                    );
                }
            }
        }
    })
}

#[test]
fn slow_simulation_autonomous_ping_pong_1x1() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            .with_invariants(vec![create_message_conservation_invariant()])
            .set_iteration_control(IterationControl::UntilAllSometimesReached(100))
            .register_workload("autonomous_server", simple_ping_pong_server)
            .register_workload("autonomous_client", autonomous_ping_pong_client)
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("Faulty seeds detected: {:?}", report.seeds_failing);
        }

        // Note: 1x1 topology won't trigger networking peer assertions
        // (peer_queue_near_capacity, peer_queue_grows, etc.)
    });
}

#[test]
fn slow_simulation_autonomous_ping_pong_1x2() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            .with_invariants(vec![create_message_conservation_invariant()])
            .set_iteration_control(IterationControl::UntilAllSometimesReached(100))
            .register_workload_count("server", 2, simple_ping_pong_server)
            .register_workload_count("client", 1, autonomous_ping_pong_client)
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("Faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}

#[test]
fn slow_simulation_autonomous_ping_pong_1x10() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            .with_invariants(vec![create_message_conservation_invariant()])
            .set_iteration_control(IterationControl::UntilAllSometimesReached(100))
            .register_workload_count("server", 10, simple_ping_pong_server)
            .register_workload_count("client", 1, autonomous_ping_pong_client)
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("Faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}

#[test]
fn slow_simulation_autonomous_ping_pong_2x2() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            .with_invariants(vec![create_message_conservation_invariant()])
            .set_iteration_control(IterationControl::UntilAllSometimesReached(100))
            .register_workload_count("server", 2, simple_ping_pong_server)
            .register_workload_count("client", 2, autonomous_ping_pong_client)
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("Faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}

#[test]
fn slow_simulation_autonomous_ping_pong_10x10() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            .with_invariants(vec![create_message_conservation_invariant()])
            .set_iteration_control(IterationControl::UntilAllSometimesReached(100))
            .register_workload_count("server", 10, simple_ping_pong_server)
            .register_workload_count("client", 10, autonomous_ping_pong_client)
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("Faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}

#[test]
fn slow_simulation_autonomous_ping_pong_random_topology() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            .with_invariants(vec![create_message_conservation_invariant()])
            .set_iteration_control(IterationControl::UntilAllSometimesReached(100))
            .with_random_topology(|builder, random| {
                // Generate random topology for each simulation seed
                let num_servers = random.random_range(1..21) as usize;
                let num_clients = random.random_range(1..21) as usize;

                tracing::info!(
                    "Random topology: {} servers, {} clients",
                    num_servers,
                    num_clients
                );

                builder
                    .register_workload_count("server", num_servers, simple_ping_pong_server)
                    .register_workload_count("client", num_clients, autonomous_ping_pong_client)
            })
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("Faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}

#[test]
#[ignore] // Run manually to debug specific seed
fn test_debug_failing_seed() {
    // Enable WARN logging to see our correlation_id traces
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Use the latest failing seed
        let failing_seed = 10888980768268317871u64;

        eprintln!("===  Debugging seed: {} ===", failing_seed);
        eprintln!("=== Expected issue: 303 pings received > 207 sent (96 duplicates) ===");

        let report = SimulationBuilder::new()
            .set_debug_seeds(vec![failing_seed])
            .with_invariants(vec![create_message_conservation_invariant()])
            .set_iteration_control(IterationControl::FixedCount(1))
            .register_workload("autonomous_server_1", simple_ping_pong_server)
            .register_workload("autonomous_server_2", simple_ping_pong_server)
            .register_workload("autonomous_client_1", autonomous_ping_pong_client)
            .register_workload("autonomous_client_2", autonomous_ping_pong_client)
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!(
                "Seed {} failed as expected - analyze trace above",
                failing_seed
            );
        }
    });
}

// ============================================================================
// TOKIO RUNNER TESTS (PRODUCTION NETWORKING VALIDATION)
// ============================================================================

/// Non-deterministic random provider for TokioRunner
#[derive(Clone)]
struct TokioRandomProvider;

impl RandomProvider for TokioRandomProvider {
    fn random<T>(&self) -> T
    where
        rand::distr::StandardUniform: rand::distr::Distribution<T>,
    {
        use rand::Rng;
        rand::rng().random()
    }

    fn random_range<T>(&self, range: std::ops::Range<T>) -> T
    where
        T: rand::distr::uniform::SampleUniform + PartialOrd,
    {
        use rand::Rng;
        rand::rng().random_range(range)
    }

    fn random_ratio(&self) -> f64 {
        use rand::Rng;
        rand::rng().random()
    }

    fn random_bool(&self, probability: f64) -> bool {
        use rand::Rng;
        rand::rng().random_bool(probability)
    }
}

/// Adapter function to run autonomous server with TokioRunner signature
async fn tokio_autonomous_server(
    provider: TokioNetworkProvider,
    time_provider: TokioTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    simple_ping_pong_server(
        TokioRandomProvider,
        provider,
        time_provider,
        task_provider,
        topology,
    )
    .await
}

/// Adapter function to run autonomous client with TokioRunner signature
async fn tokio_autonomous_client(
    provider: TokioNetworkProvider,
    time_provider: TokioTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    autonomous_ping_pong_client(
        TokioRandomProvider,
        provider,
        time_provider,
        task_provider,
        topology,
    )
    .await
}

#[test]
fn test_autonomous_ping_pong_2x2_with_tokio_runner() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    let report = local_runtime.block_on(async move {
        TokioRunner::new()
            .register_workload_count("server", 2, tokio_autonomous_server)
            .register_workload_count("client", 2, tokio_autonomous_client)
            .run()
            .await
    });

    println!("{}", report);

    assert_eq!(
        report.successful, 4,
        "All 2 servers and 2 clients should succeed"
    );
    assert_eq!(report.failed, 0, "No failures expected");
    assert_eq!(report.success_rate(), 100.0);

    println!("✅ TokioRunner 2x2 autonomous ping-pong test completed successfully");
}
