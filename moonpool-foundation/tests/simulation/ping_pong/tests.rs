use moonpool_foundation::{
    SimulationBuilder, SimulationMetrics, SimulationResult, TokioNetworkProvider, TokioRunner,
    TokioTaskProvider, TokioTimeProvider, WorkloadTopology,
    assertions::panic_on_assertion_violations, runner::IterationControl,
};
use tracing::Level;
use tracing_subscriber;

use super::actors::{PingPongClientActor, PingPongServerActor};

/// Ping count per client to keep test time reasonable
static MAX_PING_PER_CLIENT: usize = 10;

/// Creates the comprehensive message conservation invariant for ping-pong testing.
///
/// This invariant includes 7 bug detectors:
/// 1. Server receives ≤ client sends (detects message duplication)
/// 2. Server sends ≤ server receives (detects duplicate responses)
/// 3. Client receives ≤ server sends (detects response duplication)
/// 4. Client receives ≤ client sends (detects correlation bugs)
/// 5. Per-peer accounting consistency (detects misattribution)
/// 6. Global conservation with in-transit (detects message loss)
/// 7. Per-server in-transit consistency (detects internal accounting bugs)
fn create_message_conservation_invariant() -> moonpool_foundation::InvariantCheck {
    Box::new(|states, _time| {
        let mut total_pings_sent = 0u64;
        let mut total_pongs_received = 0u64;
        let mut total_pings_received = 0u64;
        let mut total_pongs_sent = 0u64;

        for (_ip, state) in states {
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
            "Message duplication detected: {} pings received by servers > {} pings sent by clients",
            total_pings_received,
            total_pings_sent
        );

        // BUG DETECTOR 2: Servers can't send more pongs than pings they received
        assert!(
            total_pongs_sent <= total_pings_received,
            "Server duplication bug: {} pongs sent > {} pings received by servers",
            total_pongs_sent,
            total_pings_received
        );

        // BUG DETECTOR 3: Clients can't receive more pongs than servers sent
        assert!(
            total_pongs_received <= total_pongs_sent,
            "Response duplication detected: {} pongs received > {} pongs sent by servers",
            total_pongs_received,
            total_pongs_sent
        );

        // BUG DETECTOR 4: Clients can't receive more pongs than pings they sent
        assert!(
            total_pongs_received <= total_pings_sent,
            "Correlation bug: {} pongs received > {} pings sent by clients",
            total_pongs_received,
            total_pings_sent
        );

        // BUG DETECTOR 5: Per-peer accounting must be consistent
        for (_ip, state) in states {
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
                            "Client per-peer ping accounting broken: sum={} != total={}",
                            sum_per_peer, total
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
                            "Client per-peer pong accounting broken: sum={} != total={}",
                            sum_per_peer, total
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
                            "Server per-peer ping accounting broken: sum={} != total={}",
                            sum_per_peer, total
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
                            "Server per-peer pong accounting broken: sum={} != total={}",
                            sum_per_peer, total
                        );
                    }
                }
            }
        }

        // BUG DETECTOR 6: Global message conservation with in-transit accounting
        let mut total_client_in_transit = 0u64;
        for (_ip, state) in states {
            if let Some(role) = state.get("role").and_then(|r| r.as_str()) {
                if role == "client" {
                    total_client_in_transit += state
                        .get("in_transit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
        }

        assert_eq!(
            total_pongs_received + total_client_in_transit,
            total_pings_sent,
            "Global conservation with client in-transit violated: {} pongs + {} client in-transit != {} pings sent",
            total_pongs_received,
            total_client_in_transit,
            total_pings_sent
        );

        // BUG DETECTOR 7: Per-server in-transit consistency
        for (_ip, state) in states {
            if let Some(role) = state.get("role").and_then(|r| r.as_str()) {
                if role == "server" {
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
                            "Server in-transit accounting broken: sum of in_transit_per_peer={} != (pings_received={} - pongs_sent={})",
                            actual_in_transit, pings_received, pongs_sent
                        );
                    }
                }
            }
        }
    })
}

/// Helper function to run a ping-pong simulation with specified topology
async fn run_ping_pong_simulation(
    num_clients: usize,
    num_servers: usize,
    max_iterations: usize,
) -> moonpool_foundation::SimulationReport {
    let mut builder = SimulationBuilder::new()
        .use_random_config()
        .with_invariants(vec![create_message_conservation_invariant()])
        .set_iteration_control(IterationControl::UntilAllSometimesReached(max_iterations));

    // Register servers
    for i in 1..=num_servers {
        builder = builder.register_workload(&format!("ping_pong_server_{}", i), ping_pong_server);
    }

    // Register clients
    for i in 1..=num_clients {
        builder = builder.register_workload(&format!("ping_pong_client_{}", i), ping_pong_client);
    }

    builder.run().await
}

/// Server workload for ping-pong communication
async fn ping_pong_server(
    _random: moonpool_foundation::random::sim::SimRandomProvider,
    provider: moonpool_foundation::SimNetworkProvider,
    time_provider: moonpool_foundation::SimTimeProvider,
    task_provider: moonpool_foundation::TokioTaskProvider,
    topology: moonpool_foundation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut server_actor =
        PingPongServerActor::new(provider, time_provider, task_provider, topology);
    server_actor.run().await
}

/// Client workload for ping-pong communication
async fn ping_pong_client(
    random: moonpool_foundation::random::sim::SimRandomProvider,
    provider: moonpool_foundation::SimNetworkProvider,
    time_provider: moonpool_foundation::SimTimeProvider,
    task_provider: moonpool_foundation::TokioTaskProvider,
    topology: moonpool_foundation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let peer_config = generate_random_peer_config(&random);

    // Client connects to all servers (workloads with "ping_pong_server" prefix)
    let server_peers = topology.get_peers_with_prefix("ping_pong_server");
    let server_topology = moonpool_foundation::WorkloadTopology {
        my_ip: topology.my_ip,
        peer_ips: server_peers.iter().map(|(_, ip)| ip.clone()).collect(),
        peer_names: server_peers.iter().map(|(name, _)| name.clone()).collect(),
        shutdown_signal: topology.shutdown_signal,
        state_registry: topology.state_registry.clone(),
    };

    let mut client_actor = ConfigurablePingPongClientActor::new(
        provider,
        time_provider,
        task_provider,
        server_topology,
        peer_config,
        random,
    );
    client_actor.run().await
}

/// Client actor with configurable ping count
struct ConfigurablePingPongClientActor<
    N: moonpool_foundation::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::TaskProvider + Clone + 'static,
    R: moonpool_foundation::random::RandomProvider,
> {
    inner: PingPongClientActor<N, T, TP, R>,
}

impl<
    N: moonpool_foundation::NetworkProvider + Clone + 'static,
    T: moonpool_foundation::TimeProvider + Clone + 'static,
    TP: moonpool_foundation::TaskProvider + Clone + 'static,
    R: moonpool_foundation::random::RandomProvider,
> ConfigurablePingPongClientActor<N, T, TP, R>
{
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
        topology: WorkloadTopology,
        peer_config: moonpool_foundation::PeerConfig,
        random: R,
    ) -> Self {
        Self {
            inner: PingPongClientActor::new(
                network,
                time,
                task_provider,
                topology,
                peer_config,
                random,
            ),
        }
    }

    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        let mut ping_count = 0;

        while ping_count < MAX_PING_PER_CLIENT {
            match self.inner.run_single_ping().await {
                Ok(true) => {
                    ping_count += 1;
                }
                Ok(false) => {
                    // Shutdown signal received
                    break;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        tracing::debug!("Client: Completed {} ping-pong exchanges", ping_count);
        Ok(SimulationMetrics::default())
    }
}

/// Generate random PeerConfig to create queue pressure and back-pressure scenarios
fn generate_random_peer_config(
    random: &moonpool_foundation::random::sim::SimRandomProvider,
) -> moonpool_foundation::PeerConfig {
    use moonpool_foundation::random::RandomProvider;
    use std::time::Duration;

    moonpool_foundation::PeerConfig {
        // Very small queue sizes to trigger near_capacity assertions
        max_queue_size: random.random_range(2..10),

        // Variable connection timeouts
        connection_timeout: Duration::from_millis(random.random_range(100..5000)),

        // Random reconnect delays
        initial_reconnect_delay: Duration::from_millis(random.random_range(10..500)),
        max_reconnect_delay: Duration::from_secs(random.random_range(1..30)),

        // Unlimited retries for simulation
        max_connection_failures: None,
    }
}

// ============================================================================
// SIMULATION TESTS WITH VARIOUS TOPOLOGIES
// ============================================================================

#[test]
fn slow_simulation_ping_pong_1x1_one() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = run_ping_pong_simulation(1, 1, 10_000).await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }
    });
}

#[test]
fn slow_simulation_ping_pong_1x1() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = run_ping_pong_simulation(1, 1, 10_000).await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        // Note: 1x1 topology won't trigger multi-server/multi-client assertions
        // (client_switches_servers, server_handles_multiple_connections, etc.)
    });
}

#[test]
fn slow_simulation_ping_pong_1x2() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = run_ping_pong_simulation(1, 2, 10_000).await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}

#[test]
fn slow_simulation_ping_pong_1x10() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = run_ping_pong_simulation(1, 10, 10_000).await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        // Note: 1xN topology won't trigger server_handles_multiple_connections assertion
    });
}

#[test]
fn slow_simulation_ping_pong_2x2() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = run_ping_pong_simulation(2, 2, 10_000).await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}

#[test]
fn slow_simulation_ping_pong_10x10() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = run_ping_pong_simulation(10, 10, 10_000).await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}

// ============================================================================
// TOKIO RUNNER TESTS (NON-DETERMINISTIC)
// ============================================================================

#[test]
fn test_ping_pong_2x2_with_tokio_runner() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::DEBUG)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    let report = local_runtime.block_on(async move {
        TokioRunner::new()
            .register_workload("ping_pong_server_1", tokio_ping_pong_server)
            .register_workload("ping_pong_server_2", tokio_ping_pong_server)
            .register_workload("ping_pong_client_1", tokio_ping_pong_client)
            .register_workload("ping_pong_client_2", tokio_ping_pong_client)
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

    println!("✅ TokioRunner 2x2 ping-pong test completed successfully");
}

/// Adapter function to run server actor with TokioRunner signature
async fn tokio_ping_pong_server(
    provider: TokioNetworkProvider,
    time_provider: TokioTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut server_actor =
        PingPongServerActor::new(provider, time_provider, task_provider, topology);
    server_actor.run().await
}

/// Adapter function to run client actor with TokioRunner signature
async fn tokio_ping_pong_client(
    provider: TokioNetworkProvider,
    time_provider: TokioTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let peer_config = moonpool_foundation::PeerConfig::default();

    struct TokioRandomProvider;
    impl moonpool_foundation::random::RandomProvider for TokioRandomProvider {
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
    impl Clone for TokioRandomProvider {
        fn clone(&self) -> Self {
            Self
        }
    }

    let random = TokioRandomProvider;

    let server_peers = topology.get_peers_with_prefix("ping_pong_server");
    let server_topology = moonpool_foundation::WorkloadTopology {
        my_ip: topology.my_ip,
        peer_ips: server_peers.iter().map(|(_, ip)| ip.clone()).collect(),
        peer_names: server_peers.iter().map(|(name, _)| name.clone()).collect(),
        shutdown_signal: topology.shutdown_signal,
        state_registry: topology.state_registry.clone(),
    };

    let mut client_actor = ConfigurablePingPongClientActor::new(
        provider,
        time_provider,
        task_provider,
        server_topology,
        peer_config,
        random,
    );
    client_actor.run().await
}
