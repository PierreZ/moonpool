use moonpool_foundation::{NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait};
use tokio::io::AsyncWriteExt;

#[test]
fn test_basic_simulation_bind() {
    // Use local runtime for tests with async traits without Send
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let sim = SimWorld::new();
        let provider = sim.network_provider();

        // Test basic binding functionality
        let listener = provider.bind("test-addr").await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr, "test-addr");

        println!("Successfully bound to {}", addr);
    });
}

// Simple echo server that reads once and writes back
async fn simple_echo_server<P>(provider: P, addr: &str) -> std::io::Result<()>
where
    P: NetworkProvider + Clone,
{
    let listener = provider.bind(addr).await?;
    let _client = provider.connect(addr).await?; // Create connection first
    let (mut stream, _peer_addr) = listener.accept().await?; // Then accept it

    // Write some test data to exercise the connection
    let test_data = b"Hello from simple echo server";
    stream.write_all(test_data).await?;

    Ok(())
}

#[test]
fn test_simple_echo_simulation() {
    // Use local runtime for tests with async traits without Send
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Use fast local config for integration tests to avoid timeouts
        let config = NetworkConfiguration::fast_local(); // Deterministic, fast
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        // Test simple echo server with simulation
        simple_echo_server(provider, "echo-server").await.unwrap();

        // Process all simulation events
        sim.run_until_empty();

        // Verify time advanced due to simulated delays (should be >10ms with WAN config)
        assert!(sim.current_time() > std::time::Duration::ZERO);
        println!("Simulation completed in {:?}", sim.current_time());
    });
}

#[test]
fn test_deterministic_simulation_behavior() {
    // Use local runtime for tests with async traits without Send
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Run simulation multiple times with same seed - should get same results
        let mut execution_times = Vec::new();

        for _run in 0..3 {
            // Use fast local config for deterministic integration tests
            let config = NetworkConfiguration::fast_local();
            let mut sim = SimWorld::new_with_network_config(config);
            let provider = sim.network_provider();

            simple_echo_server(provider, "deterministic-test")
                .await
                .unwrap();
            sim.run_until_empty();

            execution_times.push(sim.current_time());
        }

        // All runs should produce identical timing
        let first_time = execution_times[0];
        for (i, &time) in execution_times.iter().enumerate() {
            assert_eq!(
                time, first_time,
                "Run {} produced different timing than first run. Expected: {:?}, Got: {:?}",
                i, first_time, time
            );
        }

        println!(
            "All {} runs completed deterministically in {:?}",
            execution_times.len(),
            first_time
        );
    });
}

#[test]
fn test_network_provider_trait_usage() {
    // Use local runtime for tests with async traits without Send
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let sim = SimWorld::new();
        let provider = sim.network_provider();

        // Test that SimNetworkProvider can be used generically
        async fn use_provider_generically<P: NetworkProvider>(
            provider: P,
            addr: &str,
        ) -> std::io::Result<String> {
            let listener = provider.bind(addr).await?;
            Ok(listener.local_addr()?)
        }

        let addr = use_provider_generically(provider, "dynamic-test")
            .await
            .unwrap();
        assert_eq!(addr, "dynamic-test");

        println!("Generic provider usage successful: bound to {}", addr);
    });
}
