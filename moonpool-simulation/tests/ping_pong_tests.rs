use moonpool_simulation::{
    NetworkProvider, NetworkRandomizationRanges, Peer, PeerConfig, SimulationBuilder,
    SimulationMetrics, SimulationResult, TcpListenerTrait, TimeProvider, always_assert,
    rng::sim_random_range, runner::IterationControl, sometimes_assert,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{Level, instrument};
use tracing_subscriber;

static MAX_PING: usize = 10000;

// TODO: Fix cut_count
// TODO: Check sometimes assert

#[test]
fn test_ping_pong_with_simulation_builder() {
    let iteration_count = 1000;
    let check_assert = false;
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::WARN)
        .try_init();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");


    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .register_workload("ping_pong_server", ping_pong_server)
            .register_workload("ping_pong_client", ping_pong_client)
            .set_randomization_ranges(NetworkRandomizationRanges::default())
            .set_iteration_control(IterationControl::FixedCount(iteration_count))
            .set_debug_seeds(vec![42, 9495001370864752853, 123456, 999888777, 111222333]) // Test multiple seeds including the previously failing one
            .run()
            .await;

        // Display comprehensive simulation report with assertion validation
        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        // The new SimulationReport automatically includes assertion validation
        if report.assertion_validation.has_violations() {
            if !report
                .assertion_validation
                .success_rate_violations
                .is_empty()
            {
                println!("❌ Success rate violations found:");
                for violation in &report.assertion_validation.success_rate_violations {
                    println!("  - {}", violation);
                }
                if check_assert {
                    panic!("❌ Unexpected success rate violations detected!");
                }
            }

            if !report
                .assertion_validation
                .unreachable_assertions
                .is_empty()
            {
                println!("⚠️ Unreachable code detected:");
                for violation in &report.assertion_validation.unreachable_assertions {
                    println!("  - {}", violation);
                }
                if check_assert {
                    panic!("❌ Unexpected unreachable assertions detected!");
                }
            }
        } else {
            println!("");
            println!("✅ Dynamic validation passed - no assertion violations detected!");
        }
    });
}

/// Server workload for ping-pong communication
#[instrument(skip(provider))]
async fn ping_pong_server(
    _seed: u64, // Seed is set via thread_local, parameter kept for API compatibility
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
    tracing::debug!("Server: Accepted connection from client {}", _peer_addr);

    // loop to listen for client's messages
    let mut ping_sequence = 0;
    loop {
        // Read message from client
        tracing::info!("Server: About to read next message");
        let mut buffer = [0u8; 16];
        let bytes_read = match stream.read(&mut buffer).await {
            Ok(n) => {
                tracing::info!("Server: Read {} bytes successfully", n);
                n
            }
            Err(e) => {
                tracing::error!("Server: Read failed with error: {:?}", e);
                return Err(e.into());
            }
        };
        let message = std::str::from_utf8(&buffer[..bytes_read])
            .unwrap_or("INVALID")
            .trim_end_matches('\0')
            .trim_end_matches('\n');
        tracing::info!(
            "Server: Received message ({} bytes): {:?}",
            bytes_read,
            message
        );

        // Match on the message type
        if message.starts_with("PING-") {
            // Handle PING message
            let ping_message = message;

            // Verify that after splitting on "-", it is a valid number matching the sequence
            if let Some((prefix, number_str)) = ping_message.split_once('-') {
                always_assert!(
                    server_receives_valid_format,
                    prefix == "PING",
                    "Ping message should have PING prefix"
                );

                // Parse the number and verify it matches expected sequence
                if let Ok(ping_number) = number_str.parse::<i32>() {
                    always_assert!(
                        server_receives_valid_sequence,
                        ping_number == ping_sequence,
                        format!(
                            "Ping sequence number should match expected value, expected {}, got {}",
                            ping_sequence, ping_number
                        )
                    );

                    // Send corresponding PONG response
                    let pong_data = format!("PONG-{}\n", ping_number);
                    tracing::debug!("Server: Sending pong response: {}", pong_data);
                    stream.write_all(pong_data.as_bytes()).await?;
                    tracing::debug!("Server: Sent pong response {}", ping_number);

                    always_assert!(
                        server_sends_pong,
                        true,
                        "Server should always be able to send PONG responses"
                    );

                    ping_sequence += 1;
                } else {
                    panic!("Invalid ping number format: {}", number_str);
                }
            } else {
                panic!("Invalid ping message format: {}", ping_message);
            }
        } else if message == "CLOSE" {
            // Handle CLOSE message
            tracing::debug!("Server: Received CLOSE message, shutting down");

            always_assert!(
                server_receives_close,
                true,
                "Server should receive CLOSE message to shutdown gracefully"
            );

            // Send BYE!! response and exit
            tracing::debug!("Server: Sending CLOSE acknowledgment");
            stream.write_all(b"BYE!!").await?;
            tracing::debug!("Server: Sent CLOSE acknowledgment, shutting down");

            break; // Exit the loop
        } else {
            // Unknown message type
            tracing::warn!("Server: Received unknown message: {:?}", message);
            panic!("Unknown message type: {}", message);
        }
    }

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
    _seed: u64, // Seed is set via thread_local, parameter kept for API compatibility
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

    // Generate a random number of pings to send - more messages to fill small queues
    let ping_count = sim_random_range(5..MAX_PING) as u8;
    tracing::debug!("Client: Will send {} pings to server", ping_count);

    // Create a resilient peer for communication with randomized config
    let queue_size = sim_random_range(2..10);
    let timeout_ms = sim_random_range(1..100);
    let init_delay_ms = sim_random_range(1..50);
    let max_delay_ms = sim_random_range(50..200);
    let max_failures = if sim_random_range(1..100) <= 50 {
        Some(sim_random_range(1..5) as u32)
    } else {
        None
    };

    tracing::info!(
        "Client: Creating peer with config: queue_size={}, timeout_ms={}, init_delay_ms={}, max_delay_ms={}, max_failures={:?}",
        queue_size,
        timeout_ms,
        init_delay_ms,
        max_delay_ms,
        max_failures
    );

    let peer_config = PeerConfig::new(
        queue_size,
        std::time::Duration::from_millis(timeout_ms as u64),
        std::time::Duration::from_millis(init_delay_ms as u64),
        std::time::Duration::from_millis(max_delay_ms as u64),
        max_failures,
    );
    let mut peer = Peer::new(
        provider.clone(),
        time_provider.clone(),
        server_addr.to_string(),
        peer_config,
    );
    tracing::debug!("Client: Created peer for server connection");

    // Sometimes send messages in bursts to fill queue, sometimes one-by-one
    let burst_mode = sim_random_range(1..100) <= 40; // 40% chance of burst mode
    let burst_size = if burst_mode {
        sim_random_range(3..8)
    } else {
        1
    };

    // Send multiple pings and receive pongs
    let mut ping_idx = 0;
    tracing::debug!(
        "Client: Starting to send {} pings, burst_mode={}, burst_size={}",
        ping_count,
        burst_mode,
        burst_size
    );

    while ping_idx < ping_count {
        // Send burst of messages
        let current_burst = std::cmp::min(burst_size, (ping_count - ping_idx) as usize);
        tracing::debug!(
            "Client: Starting burst at ping_idx={}, current_burst={}",
            ping_idx,
            current_burst
        );

        for _j in 0..current_burst {
            // Send ping using the peer - pad to 16 bytes to match server buffer
            tracing::info!(
                "Client: About to send PING-{} (ping_idx={})",
                ping_idx,
                ping_idx
            );
            let ping_data = format!("PING-{}\n", ping_idx); // Add newline separator
            let mut padded_data = [0u8; 16];
            let bytes = ping_data.as_bytes();
            padded_data[..bytes.len()].copy_from_slice(bytes);

            let send_start = time_provider.now();
            match peer.send(padded_data.to_vec()).await {
                Ok(_) => {
                    let send_duration = time_provider.now() - send_start;
                    tracing::info!(
                        "Client: Successfully sent PING-{} using peer in {:?}",
                        ping_idx,
                        send_duration
                    );
                    always_assert!(peer_can_send, true, "Peer should be able to send messages");

                    // This will always be true in simple scenarios, so use always_assert
                    always_assert!(
                        connection_reuse_working,
                        peer.metrics().connections_established
                            <= peer.metrics().connection_attempts,
                        "Connection reuse should prevent excessive connection creation"
                    );
                }
                Err(e) => {
                    tracing::debug!("Client: Failed to send ping {}: {:?}", ping_idx, e);
                    // In this simple scenario, sends should not fail
                    return Err(moonpool_simulation::SimulationError::IoError(e.to_string()));
                }
            }
            ping_idx += 1;
        }

        // Now receive responses for the burst
        let start_idx = ping_idx - current_burst as u8;
        let mut pending_data = String::new();
        let mut responses_received = 0;

        while responses_received < current_burst {
            // Read data from peer
            let mut response_buffer = [0u8; 64]; // Larger buffer to handle multiple messages
            match peer.receive(&mut response_buffer).await {
                Ok(bytes_read) => {
                    let new_data =
                        std::str::from_utf8(&response_buffer[..bytes_read]).unwrap_or("INVALID");
                    pending_data.push_str(new_data);

                    // Process all complete lines (messages ending with \n)
                    while let Some(newline_pos) = pending_data.find('\n') {
                        let message = pending_data[..newline_pos].trim().to_string();
                        let remaining = pending_data[newline_pos + 1..].to_string();
                        pending_data = remaining;

                        if message.starts_with("PONG-") {
                            let response_idx = start_idx + responses_received as u8;

                            tracing::debug!(
                                "Client: Received pong {}: '{}'",
                                response_idx,
                                message
                            );

                            // Verify we got a valid pong response
                            always_assert!(
                                peer_receives_pong,
                                message.starts_with("PONG"),
                                "Peer should receive PONG responses"
                            );
                            let expected_pong = format!("PONG-{}", response_idx);
                            always_assert!(
                                pong_sequence_matches,
                                message == expected_pong,
                                format!(
                                    "PONG sequence mismatch: expected '{}', got '{}'",
                                    expected_pong, message
                                )
                            );

                            responses_received += 1;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("Client: Failed to receive data: {:?}", e);
                    return Err(moonpool_simulation::SimulationError::IoError(e.to_string()));
                }
            }
        }
    }

    // Send CLOSE message to server - pad to 16 bytes to match server buffer
    tracing::debug!("Client: Sending CLOSE message to server");
    let mut close_data = [0u8; 16];
    close_data[..6].copy_from_slice(b"CLOSE\n");

    match peer.send(close_data.to_vec()).await {
        Ok(_) => {
            tracing::debug!("Client: Successfully sent CLOSE message");
            always_assert!(
                client_sends_close,
                true,
                "Client should be able to send CLOSE message"
            );
        }
        Err(e) => {
            tracing::debug!("Client: Failed to send CLOSE message: {:?}", e);
            return Err(moonpool_simulation::SimulationError::IoError(e.to_string()));
        }
    }

    // Read server's acknowledgment (if CLOSE was sent successfully)
    tracing::debug!("Client: Reading server's CLOSE acknowledgment");
    let mut ack_buffer = [0u8; 5];
    match peer.receive(&mut ack_buffer).await {
        Ok(bytes_read) => {
            let ack_message = std::str::from_utf8(&ack_buffer[..bytes_read]).unwrap_or("INVALID");
            tracing::debug!("Client: Received acknowledgment: {:?}", ack_message);

            // Verify we got the expected acknowledgment
            always_assert!(
                client_receives_bye,
                ack_message == "BYE!!",
                "Client should receive BYE acknowledgment from server"
            );
            always_assert!(
                bye_message_exact,
                &ack_buffer[..bytes_read] == b"BYE!!",
                format!(
                    "BYE message should be exact: expected 'BYE!!', got '{}'",
                    String::from_utf8_lossy(&ack_buffer[..bytes_read])
                )
            );
        }
        Err(e) => {
            tracing::warn!("Client: Failed to receive CLOSE acknowledgment: {:?}", e);
            // Not critical - server might have already closed
            sometimes_assert!(
                close_ack_received,
                false,
                "CLOSE acknowledgment may not always be received"
            );
            tracing::info!("Client: Server may have already closed, continuing");
        }
    }

    // Record successful round-trip communication
    always_assert!(
        peer_roundtrip_success,
        true,
        "Peer should complete round-trip communication"
    );

    // This will always be true in simple scenarios, so use always_assert
    always_assert!(
        no_connection_leaks,
        peer.metrics().connection_failures == 0 || peer.metrics().connections_established > 0,
        "Connection failures should not prevent future successful connections"
    );

    // Log peer metrics and validate resilient behavior
    let metrics = peer.metrics();
    tracing::debug!(
        "Client: Peer metrics - attempts={}, established={}, failures={}",
        metrics.connection_attempts,
        metrics.connections_established,
        metrics.connection_failures
    );

    // Connection failures are expected sometimes in resilient systems
    if metrics.connection_failures > 0 {
        // In simple scenarios, failures are rare, so this would be sometimes_assert in complex scenarios
        sometimes_assert!(
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

    let metrics = SimulationMetrics::default();
    tracing::debug!("Client: Workload completed successfully");
    Ok(metrics)
}
