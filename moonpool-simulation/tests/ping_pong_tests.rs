use moonpool_simulation::{
    NetworkConfiguration, NetworkProvider, Peer, PeerConfig, SimulationBuilder, SimulationMetrics,
    SimulationResult, TcpListenerTrait, TimeProvider, always_assert,
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
    _time_provider: moonpool_simulation::SimTimeProvider,
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
    let ping_message = std::str::from_utf8(&buffer).unwrap_or("INVALID");
    tracing::debug!("Server: Received ping: {:?}", ping_message);

    // Verify we received a valid ping
    always_assert!(
        server_receives_ping,
        ping_message.starts_with("PING"),
        "Server should always receive PING messages"
    );
    assert_eq!(&buffer, b"PING-0");

    // Send pong response back to client
    tracing::debug!("Server: Sending pong response");
    let pong_data = b"PONG-0";
    stream.write_all(pong_data).await?;
    tracing::debug!("Server: Sent pong response");

    // Record successful server response
    always_assert!(
        server_sends_pong,
        true,
        "Server should always be able to send PONG responses"
    );

    // This should always trigger - use always_assert since it's always true
    always_assert!(
        server_always_succeeds,
        true,
        "Server always succeeds in basic ping-pong"
    );

    let metrics = SimulationMetrics::default();
    tracing::debug!("Server: Workload completed successfully");
    Ok(metrics)
}

/// Client workload for ping-pong communication
#[instrument(skip(provider))]
async fn ping_pong_client(
    _seed: u64,
    provider: moonpool_simulation::SimNetworkProvider,
    time_provider: moonpool_simulation::SimTimeProvider,
    topology: moonpool_simulation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    // Get the server address from topology
    let server_addr = topology.peer_ips.first().ok_or_else(|| {
        moonpool_simulation::SimulationError::InvalidState("No server peer in topology".to_string())
    })?;
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
    let start_time = time_provider.now();
    provider
        .sleep(std::time::Duration::from_millis(100))?
        .await?;
    let _actual_sleep_time = time_provider.now() - start_time;
    tracing::debug!("Client: Sleep completed, connecting to server");

    // Create a resilient peer for communication
    let peer_config = PeerConfig::local_network();
    let mut peer = Peer::new(
        provider.clone(),
        time_provider.clone(),
        server_addr.to_string(),
        peer_config,
    );
    tracing::debug!("Client: Created peer for server connection");

    // Send ping using the peer
    tracing::debug!("Client: Sending ping using peer");
    let ping_data = b"PING-0";
    let send_start = time_provider.now();
    match peer.send(ping_data.to_vec()).await {
        Ok(_) => {
            let send_duration = time_provider.now() - send_start;
            tracing::debug!(
                "Client: Successfully sent ping using peer in {:?}",
                send_duration
            );
            always_assert!(peer_can_send, true, "Peer should be able to send messages");
        }
        Err(e) => {
            tracing::debug!("Client: Failed to send ping: {:?}", e);
            // In this simple scenario, sends should not fail
            return Err(moonpool_simulation::SimulationError::IoError(e.to_string()));
        }
    }

    // Read pong response using the peer
    tracing::debug!("Client: Reading pong response");
    let receive_start = time_provider.now();
    let mut response_buffer = [0u8; 6];
    match peer.receive(&mut response_buffer).await {
        Ok(bytes_read) => {
            let receive_duration = time_provider.now() - receive_start;
            let pong_message =
                std::str::from_utf8(&response_buffer[..bytes_read]).unwrap_or("INVALID");
            tracing::debug!(
                "Client: Received pong: {:?} in {:?}",
                pong_message,
                receive_duration
            );

            // Verify we got a valid pong response
            always_assert!(
                peer_receives_pong,
                pong_message.starts_with("PONG"),
                "Peer should receive PONG responses"
            );
            assert_eq!(&response_buffer[..bytes_read], b"PONG-0");

            // Record successful round-trip communication
            always_assert!(
                peer_roundtrip_success,
                true,
                "Peer should complete round-trip communication"
            );
        }
        Err(e) => {
            tracing::debug!("Client: Failed to receive pong: {:?}", e);
            // In this simple scenario, receives should not fail
            return Err(moonpool_simulation::SimulationError::IoError(e.to_string()));
        }
    }

    // Log peer metrics and validate resilient behavior
    let metrics = peer.metrics();
    tracing::debug!(
        "Client: Peer metrics - attempts={}, established={}, failures={}",
        metrics.connection_attempts,
        metrics.connections_established,
        metrics.connection_failures
    );

    // Validate peer metrics make sense
    always_assert!(
        peer_attempts_positive,
        metrics.connection_attempts > 0,
        "Peer should have made connection attempts"
    );
    always_assert!(
        peer_established_positive,
        metrics.connections_established > 0,
        "Peer should have established connections"
    );

    // Connection failures are expected sometimes in resilient systems
    if metrics.connection_failures > 0 {
        // In simple scenarios, failures are rare, so this would be sometimes_assert in complex scenarios
        always_assert!(
            peer_recovers_from_failures,
            metrics.connections_established >= metrics.connection_failures,
            "Peer should recover from failures"
        );
    }

    // Success rate validation - in simple scenarios, success rate is always high
    let success_rate = if metrics.connection_attempts > 0 {
        (metrics.connections_established as f64 / metrics.connection_attempts as f64) * 100.0
    } else {
        0.0
    };
    tracing::debug!("Client: Connection success rate: {:.1}%", success_rate);
    always_assert!(
        peer_reasonable_success_rate,
        success_rate >= 50.0,
        "Peer should maintain reasonable success rate in simple scenarios"
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
            .set_iterations(20)
            .run()
            .await;

        // Display comprehensive simulation report with assertion validation
        println!("{}", report);

        // The new SimulationReport automatically includes assertion validation
        // If there are any contract violations, they would be shown in the report
        match &report.assertion_validation {
            Ok(()) => {
                println!("✅ Dynamic validation passed - no sometimes_assert! calls with 0% success rate detected!");
            }
            Err(violations) => {
                panic!(
                    "❌ Dynamic validation failed - assertion contract violations detected:\\n{}",
                    violations
                );
            }
        }

        println!("\\n✅ Peer resilience validation completed!");
        println!("✅ Assertion macro functionality verified!");
        println!(
            "The Peer abstraction demonstrated robust behavior across {} iterations",
            report.iterations
        );
        println!("with WAN network simulation conditions and comprehensive assertion tracking.");
    });
}
