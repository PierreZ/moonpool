use moonpool_simulation::{
    SimulationBuilder, SimulationMetrics, SimulationResult, TokioNetworkProvider, TokioRunner,
    TokioTaskProvider, TokioTimeProvider, WorkloadTopology,
    assertions::panic_on_assertion_violations, runner::IterationControl,
};
use tracing::Level;
use tracing_subscriber;

use super::actors::{PingPongClientActor, PingPongServerActor};

/// Reduced ping count per client to keep total test time reasonable with 2x2 setup
static MAX_PING_PER_CLIENT: usize = 10;

#[test]
fn slow_simulation_ping_pong_2x2() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        // Level should always be ERROR when searching for seeds
        .with_max_level(Level::ERROR)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()
            // Uncomment to debug specific problematic seeds:
            // .set_debug_seeds(vec![12345]) // Replace with problematic seed
            .register_workload("ping_pong_server_1", ping_pong_server)
            .register_workload("ping_pong_server_2", ping_pong_server)
            .register_workload("ping_pong_client_1", ping_pong_client)
            .register_workload("ping_pong_client_2", ping_pong_client)
            .set_iteration_control(IterationControl::UntilAllSometimesReached(10_000))
            .run()
            .await;

        // Display comprehensive simulation report with assertion validation
        println!("{}", report);

        // With chaos testing, some seeds may fail due to network disruption - this is expected
        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        // Validate assertion contracts using the helper function
        panic_on_assertion_violations(&report);
    });
}

/// Server workload for ping-pong communication with multiple clients
async fn ping_pong_server(
    _random: moonpool_simulation::random::sim::SimRandomProvider,
    provider: moonpool_simulation::SimNetworkProvider,
    time_provider: moonpool_simulation::SimTimeProvider,
    task_provider: moonpool_simulation::TokioTaskProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut server_actor =
        PingPongServerActor::new(provider, time_provider, task_provider, topology);
    server_actor.run().await
}

/// Client workload for ping-pong communication with multiple servers
async fn ping_pong_client(
    random: moonpool_simulation::random::sim::SimRandomProvider,
    provider: moonpool_simulation::SimNetworkProvider,
    time_provider: moonpool_simulation::SimTimeProvider,
    task_provider: moonpool_simulation::TokioTaskProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    // Generate random PeerConfig to create back-pressure scenarios
    let peer_config = generate_random_peer_config(&random);

    // Client connects to all servers (workloads with "ping_pong_server" prefix)
    let server_peers = topology.get_peers_with_prefix("ping_pong_server");
    let server_topology = moonpool_simulation::WorkloadTopology {
        my_ip: topology.my_ip,
        peer_ips: server_peers.iter().map(|(_, ip)| ip.clone()).collect(),
        peer_names: server_peers.iter().map(|(name, _)| name.clone()).collect(),
        shutdown_signal: topology.shutdown_signal,
    };

    let mut client_actor = MultiMultiPingPongClientActor::new(
        provider,
        time_provider,
        task_provider,
        server_topology,
        peer_config,
        random,
    );
    client_actor.run().await
}

/// Client actor with reduced ping count for 4x4 testing
struct MultiMultiPingPongClientActor<
    N: moonpool_simulation::NetworkProvider + Clone + 'static,
    T: moonpool_simulation::TimeProvider + Clone + 'static,
    TP: moonpool_simulation::TaskProvider + Clone + 'static,
    R: moonpool_simulation::random::RandomProvider,
> {
    inner: PingPongClientActor<N, T, TP, R>,
}

impl<
    N: moonpool_simulation::NetworkProvider + Clone + 'static,
    T: moonpool_simulation::TimeProvider + Clone + 'static,
    TP: moonpool_simulation::TaskProvider + Clone + 'static,
    R: moonpool_simulation::random::RandomProvider,
> MultiMultiPingPongClientActor<N, T, TP, R>
{
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
        topology: WorkloadTopology,
        peer_config: moonpool_simulation::PeerConfig,
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
        // Override the MAX_PING limit for this 4x4 test
        let mut ping_count = 0;

        while ping_count < MAX_PING_PER_CLIENT {
            // Use the inner client's logic but with our own counter
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

        tracing::debug!("2x2 client: Completed {} ping-pong exchanges", ping_count);
        Ok(SimulationMetrics::default())
    }
}

/// Generate random PeerConfig to create queue pressure and back-pressure scenarios
fn generate_random_peer_config(
    random: &moonpool_simulation::random::sim::SimRandomProvider,
) -> moonpool_simulation::PeerConfig {
    use moonpool_simulation::random::RandomProvider;
    use std::time::Duration;

    moonpool_simulation::PeerConfig {
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

#[test]
fn test_ping_pong_2x2_with_tokio_runner() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::DEBUG)
        .try_init();

    // Create single-threaded Tokio runtime for deterministic execution
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

    // Display the report
    println!("{}", report);

    // Validate that all workloads completed successfully
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
    // For TokioRunner, use default PeerConfig since we don't have RandomProvider
    let peer_config = moonpool_simulation::PeerConfig::default();

    // Create a dummy random provider that uses thread_rng
    struct TokioRandomProvider;
    impl moonpool_simulation::random::RandomProvider for TokioRandomProvider {
        fn random<T>(&self) -> T
        where
            rand::distributions::Standard: rand::distributions::Distribution<T>,
        {
            use rand::Rng;
            rand::thread_rng().r#gen()
        }

        fn random_range<T>(&self, range: std::ops::Range<T>) -> T
        where
            T: rand::distributions::uniform::SampleUniform + PartialOrd,
        {
            use rand::Rng;
            rand::thread_rng().gen_range(range)
        }

        fn random_ratio(&self) -> f64 {
            use rand::Rng;
            rand::thread_rng().r#gen()
        }

        fn random_bool(&self, probability: f64) -> bool {
            use rand::Rng;
            rand::thread_rng().gen_bool(probability)
        }
    }
    impl Clone for TokioRandomProvider {
        fn clone(&self) -> Self {
            Self
        }
    }

    let random = TokioRandomProvider;

    // Client connects to all servers (workloads with "ping_pong_server" prefix)
    let server_peers = topology.get_peers_with_prefix("ping_pong_server");
    let server_topology = moonpool_simulation::WorkloadTopology {
        my_ip: topology.my_ip,
        peer_ips: server_peers.iter().map(|(_, ip)| ip.clone()).collect(),
        peer_names: server_peers.iter().map(|(name, _)| name.clone()).collect(),
        shutdown_signal: topology.shutdown_signal,
    };

    // Create a wrapper client that runs reduced ping count
    let mut client_actor = MultiMultiPingPongClientActor::new(
        provider,
        time_provider,
        task_provider,
        server_topology,
        peer_config,
        random,
    );
    client_actor.run().await
}
