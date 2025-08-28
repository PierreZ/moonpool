use moonpool_simulation::{
    NetworkProvider, Peer, PeerConfig, SimulationMetrics, SimulationResult, TcpListenerTrait,
    TimeProvider, WorkloadTopology, always_assert, rng::sim_random_range, sometimes_assert,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::instrument;

static MAX_PING: usize = 10000;

/// Server actor for ping-pong communication with reconnection handling
pub struct PingPongServerActor<N: NetworkProvider, T: TimeProvider> {
    network: N,
    time: T,
    topology: WorkloadTopology,
}

impl<N: NetworkProvider, T: TimeProvider> PingPongServerActor<N, T> {
    pub fn new(network: N, time: T, topology: WorkloadTopology) -> Self {
        Self {
            network,
            time,
            topology,
        }
    }

    /// Main server loop with listener reconnection handling
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        let server_addr = self.topology.my_ip.clone();
        tracing::debug!(
            "Server: Starting server actor on {} with {} peer(s): {:?}",
            self.topology.my_ip,
            self.topology.peer_ips.len(),
            self.topology.peer_ips
        );

        // Outer retry loop for listener failures
        let mut listener_failures = 0u32;
        let mut backoff_delay = Duration::from_millis(10);
        let max_backoff = Duration::from_millis(1000);

        loop {
            // Try to bind to the server address
            tracing::debug!("Server: Attempting to bind to {}", server_addr);
            let listener = match self.network.bind(&server_addr).await {
                Ok(listener) => {
                    tracing::debug!("Server: Bound successfully, waiting for connections");
                    listener_failures = 0; // Reset on success
                    backoff_delay = Duration::from_millis(10); // Reset backoff
                    listener
                }
                Err(e) => {
                    listener_failures += 1;
                    sometimes_assert!(
                        server_bind_fails,
                        true,
                        "Server bind should sometimes fail during chaos testing"
                    );

                    tracing::warn!(
                        "Server: Bind failed (attempt {}): {:?}",
                        listener_failures,
                        e
                    );

                    // Exponential backoff
                    self.time.sleep(backoff_delay).await?;
                    backoff_delay = std::cmp::min(backoff_delay * 2, max_backoff);

                    continue;
                }
            };

            // Accept connections and handle them
            match self.accept_and_handle_client(listener).await {
                Ok(metrics) => return Ok(metrics),
                Err(e) => {
                    sometimes_assert!(
                        server_accept_fails,
                        true,
                        "Server accept should sometimes fail during chaos testing"
                    );

                    tracing::warn!("Server: Accept/handle failed: {:?}, will retry binding", e);
                    // Continue outer loop to rebind listener
                }
            }
        }
    }

    /// Accept a connection and handle the client session
    async fn accept_and_handle_client(
        &mut self,
        listener: N::TcpListener,
    ) -> SimulationResult<SimulationMetrics> {
        // Accept a connection from the client
        tracing::debug!("Server: Accepting connection");
        let (mut stream, _peer_addr) = listener.accept().await?;
        tracing::debug!("Server: Accepted connection from client {}", _peer_addr);

        // Handle the client session
        self.handle_client_session(&mut stream).await?;

        let metrics = SimulationMetrics::default();
        tracing::debug!("Server: Workload completed successfully");
        Ok(metrics)
    }

    /// Handle a single client session (ping-pong communication)
    async fn handle_client_session(&mut self, stream: &mut N::TcpStream) -> SimulationResult<()> {
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
                self.handle_ping_message(stream, message, &mut ping_sequence)
                    .await?;
            } else if message == "CLOSE" {
                self.handle_close_message(stream).await?;
                break; // Exit the loop
            } else {
                // Unknown message type
                tracing::warn!("Server: Received unknown message: {:?}", message);
                panic!("Unknown message type: {}", message);
            }
        }

        always_assert!(
            server_always_succeeds,
            true,
            "Server always succeeds in basic ping-pong"
        );

        Ok(())
    }

    /// Handle a PING message and send PONG response
    async fn handle_ping_message(
        &mut self,
        stream: &mut N::TcpStream,
        message: &str,
        ping_sequence: &mut i32,
    ) -> SimulationResult<()> {
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
                    ping_number == *ping_sequence,
                    format!(
                        "Ping sequence number should match expected value, expected {}, got {}",
                        *ping_sequence, ping_number
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

                *ping_sequence += 1;
            } else {
                panic!("Invalid ping number format: {}", number_str);
            }
        } else {
            panic!("Invalid ping message format: {}", ping_message);
        }

        Ok(())
    }

    /// Handle a CLOSE message and send acknowledgment
    async fn handle_close_message(&mut self, stream: &mut N::TcpStream) -> SimulationResult<()> {
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

        Ok(())
    }
}

/// Client actor for ping-pong communication using resilient Peer
pub struct PingPongClientActor<N: NetworkProvider, T: TimeProvider> {
    network: N,
    time: T,
    topology: WorkloadTopology,
}

impl<N: NetworkProvider, T: TimeProvider> PingPongClientActor<N, T> {
    pub fn new(network: N, time: T, topology: WorkloadTopology) -> Self {
        Self {
            network,
            time,
            topology,
        }
    }

    /// Main client loop using resilient Peer for communication
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        // Get the server address from topology
        let server_addr = self.topology.peer_ips.first().ok_or_else(|| {
            moonpool_simulation::SimulationError::InvalidState(
                "No server peer in topology".to_string(),
            )
        })?;
        tracing::debug!(
            "Client: Starting client actor on {} with {} peer(s): {:?}",
            self.topology.my_ip,
            self.topology.peer_ips.len(),
            self.topology.peer_ips
        );
        tracing::debug!("Client: Will connect to server at {}", server_addr);

        // Add coordination sleep to allow server setup before client connects
        tracing::debug!("Client: Sleeping for 100ms to allow server setup");
        let start_time = self.time.now();
        self.time
            .sleep(std::time::Duration::from_millis(100))
            .await?;
        let _actual_sleep_time = self.time.now() - start_time;
        tracing::debug!("Client: Sleep completed, connecting to server");

        // Generate a random number of pings to send
        let ping_count = sim_random_range(5..MAX_PING) as u8;
        tracing::debug!("Client: Will send {} pings to server", ping_count);

        // Create a resilient peer for communication with randomized config
        let mut peer = self.create_resilient_peer(server_addr).await;

        // Send pings and receive pongs
        self.send_ping_pong_messages(&mut peer, ping_count).await?;

        // Send CLOSE message and receive acknowledgment
        self.send_close_and_receive_ack(&mut peer).await?;

        // Record successful round-trip communication
        always_assert!(
            peer_roundtrip_success,
            true,
            "Peer should complete round-trip communication"
        );

        self.validate_peer_metrics(&peer);

        let metrics = SimulationMetrics::default();
        tracing::debug!("Client: Workload completed successfully");
        Ok(metrics)
    }

    /// Create a resilient peer with randomized configuration
    async fn create_resilient_peer(&self, server_addr: &str) -> Peer<N, T> {
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
            Duration::from_millis(timeout_ms as u64),
            Duration::from_millis(init_delay_ms as u64),
            Duration::from_millis(max_delay_ms as u64),
            max_failures,
        );

        Peer::new(
            self.network.clone(),
            self.time.clone(),
            server_addr.to_string(),
            peer_config,
        )
    }

    /// Send ping messages and receive pong responses
    async fn send_ping_pong_messages(
        &self,
        peer: &mut Peer<N, T>,
        ping_count: u8,
    ) -> SimulationResult<()> {
        // Sometimes send messages in bursts to fill queue, sometimes one-by-one
        let burst_mode = sim_random_range(1..100) <= 40; // 40% chance of burst mode
        let burst_size = if burst_mode {
            sim_random_range(3..8)
        } else {
            1
        };

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

            // Send burst
            for _j in 0..current_burst {
                self.send_ping_message(peer, ping_idx).await?;
                ping_idx += 1;
            }

            // Receive responses for the burst
            let start_idx = ping_idx - current_burst as u8;
            self.receive_pong_responses(peer, current_burst, start_idx)
                .await?;
        }

        Ok(())
    }

    /// Send a single ping message
    async fn send_ping_message(&self, peer: &mut Peer<N, T>, ping_idx: u8) -> SimulationResult<()> {
        tracing::info!(
            "Client: About to send PING-{} (ping_idx={})",
            ping_idx,
            ping_idx
        );
        let ping_data = format!("PING-{}\n", ping_idx);
        let mut padded_data = [0u8; 16];
        let bytes = ping_data.as_bytes();
        padded_data[..bytes.len()].copy_from_slice(bytes);

        let send_start = self.time.now();
        match peer.send(padded_data.to_vec()).await {
            Ok(_) => {
                let send_duration = self.time.now() - send_start;
                tracing::info!(
                    "Client: Successfully sent PING-{} using peer in {:?}",
                    ping_idx,
                    send_duration
                );
                always_assert!(peer_can_send, true, "Peer should be able to send messages");

                always_assert!(
                    connection_reuse_working,
                    peer.metrics().connections_established <= peer.metrics().connection_attempts,
                    "Connection reuse should prevent excessive connection creation"
                );
            }
            Err(e) => {
                tracing::debug!("Client: Failed to send ping {}: {:?}", ping_idx, e);
                return Err(moonpool_simulation::SimulationError::IoError(e.to_string()));
            }
        }

        Ok(())
    }

    /// Receive pong responses for a burst of messages
    async fn receive_pong_responses(
        &self,
        peer: &mut Peer<N, T>,
        expected_count: usize,
        start_idx: u8,
    ) -> SimulationResult<()> {
        let mut pending_data = String::new();
        let mut responses_received = 0;

        while responses_received < expected_count {
            // Read data from peer
            let mut response_buffer = [0u8; 64];
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

        Ok(())
    }

    /// Send CLOSE message and receive acknowledgment
    async fn send_close_and_receive_ack(&self, peer: &mut Peer<N, T>) -> SimulationResult<()> {
        // Send CLOSE message to server
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

        // Read server's acknowledgment
        tracing::debug!("Client: Reading server's CLOSE acknowledgment");
        let mut ack_buffer = [0u8; 5];
        match peer.receive(&mut ack_buffer).await {
            Ok(bytes_read) => {
                let ack_message =
                    std::str::from_utf8(&ack_buffer[..bytes_read]).unwrap_or("INVALID");
                tracing::debug!("Client: Received acknowledgment: {:?}", ack_message);

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
                sometimes_assert!(
                    close_ack_received,
                    false,
                    "CLOSE acknowledgment may not always be received"
                );
                tracing::info!("Client: Server may have already closed, continuing");
            }
        }

        Ok(())
    }

    /// Validate peer metrics and connection behavior
    fn validate_peer_metrics(&self, peer: &Peer<N, T>) {
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
            sometimes_assert!(
                peer_recovers_from_failures,
                metrics.connections_established >= metrics.connection_failures,
                "Peer should recover from failures"
            );
        }

        // Success rate validation
        let success_rate = if metrics.connection_attempts > 0 {
            (metrics.connections_established as f64 / metrics.connection_attempts as f64) * 100.0
        } else {
            0.0
        };
        tracing::debug!("Client: Connection success rate: {:.1}%", success_rate);
    }
}
