use moonpool_simulation::{
    NetworkConfiguration, NetworkProvider, SimulationBuilder, SimulationMetrics, SimulationResult,
    TcpListenerTrait,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{Level, info, instrument};
use tracing_subscriber;

/// Simplified ping-pong workload similar to the working echo pattern
#[instrument(skip(provider))]
async fn ping_pong_simple(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    ip_address: Option<String>,
) -> SimulationResult<SimulationMetrics> {
    // IP address assigned: use for identification or logging if needed
    let _assigned_ip = ip_address.unwrap_or_else(|| "no-ip".to_string());

    let server_addr = "ping-pong-server";

    // Follow the working pattern from simple_echo_server
    let listener = provider.bind(server_addr).await?;
    let _client = provider.connect(server_addr).await?; // Create connection first
    let (mut stream, _peer_addr) = listener.accept().await?; // Then accept it

    // Just write some ping-pong style test data to exercise the connection
    let ping_data = b"PING-0";
    let pong_data = b"PONG-0";

    // Write ping and pong data
    stream.write_all(ping_data).await?;
    stream.write_all(pong_data).await?;

    // Return metrics
    let metrics = SimulationMetrics::default();

    Ok(metrics)
}

#[test]
fn test_ping_pong_with_simulation_report() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(Level::WARN)
        .try_init();

    info!("tracing setup");

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Use manual SimWorld approach like the working tests
        let config = NetworkConfiguration::wan_simulation(); // Use WAN config for noticeable delays
        let mut sim = moonpool_simulation::SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        // Run the ping-pong workload directly
        let result = ping_pong_simple(12345, provider, Some("10.0.0.1".to_string())).await;

        // Process all simulation events
        sim.run_until_empty();

        // Extract metrics from the simulation
        let sim_metrics = sim.extract_metrics();

        match result {
            Ok(_workload_metrics) => {
                println!("Ping-pong workload completed successfully!");
                println!("Simulated time: {:?}", sim_metrics.simulated_time);
                println!("Events processed: {}", sim_metrics.events_processed);

                // Verify the simulation worked
                assert!(sim_metrics.simulated_time > std::time::Duration::ZERO);
                assert!(sim_metrics.events_processed > 0);

                println!("Manual ping-pong simulation test passed!");
            }
            Err(e) => {
                panic!("Ping-pong workload failed: {:?}", e);
            }
        }
    });
}

/// Server workload for ping-pong communication
#[instrument(skip(provider))]
async fn ping_pong_server(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    // Use the server's own IP as the bind address
    let server_addr = &topology.my_ip;
    tracing::debug!(
        "Server: Starting server workload on {} with {} peer(s): {:?}",
        topology.my_ip,
        topology.peer_ips.len(),
        topology.peer_ips
    );

    // Bind to the server address
    tracing::debug!("Server: Binding to {}", server_addr);
    let listener = provider.bind(server_addr).await?;
    tracing::debug!("Server: Bound successfully, waiting for connections");

    // Accept a connection from the client
    tracing::debug!("Server: Accepting connection");
    let (mut stream, _peer_addr) = listener.accept().await?;
    tracing::debug!("Server: Accepted connection from client");

    // Read ping from client
    tracing::debug!("Server: Reading ping from client");
    let mut buffer = [0u8; 6];
    stream.read_exact(&mut buffer).await?;
    assert_eq!(&buffer, b"PING-0");
    tracing::debug!("Server: Received ping: {:?}", std::str::from_utf8(&buffer));

    // Send pong response back to client
    tracing::debug!("Server: Sending pong response");
    let pong_data = b"PONG-0";
    stream.write_all(pong_data).await?;
    tracing::debug!("Server: Sent pong response");

    let metrics = SimulationMetrics::default();
    tracing::debug!("Server: Workload completed successfully");
    Ok(metrics)
}

/// Client workload for ping-pong communication
#[instrument(skip(provider))]
async fn ping_pong_client(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    // Connect to the first peer (assuming it's the server)
    let default_server = "ping-pong-server".to_string();
    let server_addr = topology.peer_ips.first().unwrap_or(&default_server);
    tracing::debug!(
        "Client: Starting client workload on {} with {} peer(s): {:?}",
        topology.my_ip,
        topology.peer_ips.len(),
        topology.peer_ips
    );
    tracing::debug!("Client: Will connect to server at {}", server_addr);

    // Add coordination sleep to allow server setup before client connects
    // This prevents race conditions in concurrent workload execution
    tracing::debug!("Client: Sleeping for 100ms to allow server setup");
    provider
        .sleep(std::time::Duration::from_millis(100))?
        .await?;
    tracing::debug!("Client: Sleep completed, connecting to server");

    // Connect to the server
    tracing::debug!("Client: Connecting to {}", server_addr);
    let mut client = provider.connect(server_addr).await?;
    tracing::debug!("Client: Connected successfully");

    // Send ping to server
    tracing::debug!("Client: Sending ping");
    let ping_data = b"PING-0";
    client.write_all(ping_data).await?;
    tracing::debug!("Client: Sent ping: {:?}", std::str::from_utf8(ping_data));

    // Read pong response from server
    tracing::debug!("Client: Reading pong response");
    let mut response_buffer = [0u8; 6];
    client.read_exact(&mut response_buffer).await?;
    assert_eq!(&response_buffer, b"PONG-0");
    tracing::debug!(
        "Client: Received pong: {:?}",
        std::str::from_utf8(&response_buffer)
    );

    let metrics = SimulationMetrics::default();
    tracing::debug!("Client: Workload completed successfully");
    Ok(metrics)
}

#[test]
fn test_ping_pong_with_simulation_builder() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG) // Enable debug logging
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .set_network_config(NetworkConfiguration::wan_simulation())
            .register_workload("ping_pong_server", ping_pong_server)
            .register_workload("ping_pong_client", ping_pong_client)
            .set_iterations(1)
            .set_seeds(vec![12345])
            .run()
            .await;

        // Verify the simulation completed successfully
        assert_eq!(report.iterations, 1);
        assert_eq!(report.successful_runs, 1);
        assert_eq!(report.failed_runs, 0);
        assert_eq!(report.success_rate(), 100.0);

        // Verify simulation time advanced (indicating events were processed)
        assert!(report.average_simulated_time() > std::time::Duration::ZERO);
        assert!(report.average_events_processed() > 0.0);

        println!("Separate client-server ping-pong with SimulationBuilder test passed!");
        println!("Simulation report:\n{}", report);
    });
}
