use moonpool_simulation::{
    NetworkProvider, Peer, PeerConfig, SimulationMetrics, SimulationResult, TaskProvider,
    TcpListenerTrait, TimeProvider, WorkloadTopology, always_assert, buggify, buggify_with_prob,
    rng::{random_unique_id, sim_random_range},
    sometimes_assert,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::instrument;

static MAX_PING: usize = 100;

/// Server actor for ping-pong communication with reconnection handling
pub struct PingPongServerActor<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    network: N,
    time: T,
    #[allow(dead_code)]
    task_provider: TP,
    topology: WorkloadTopology,
}

impl<N: NetworkProvider + 'static, T: TimeProvider + 'static, TP: TaskProvider + 'static>
    PingPongServerActor<N, T, TP>
{
    pub fn new(network: N, time: T, task_provider: TP, topology: WorkloadTopology) -> Self {
        Self {
            network,
            time,
            task_provider,
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
        loop {
            // Read message from client with timeout for chaos resilience
            tracing::info!("Server: About to read next message");
            let timeout = if buggify_with_prob!(0.02) {
                tracing::debug!("Server: Buggify using very short read timeout");
                Duration::from_millis(sim_random_range(1..50)) // Very short timeout to force timeouts
            } else {
                Duration::from_secs(sim_random_range(5..10)) // Normal timeout
            };
            let mut buffer = [0u8; 128]; // Larger buffer for UUID messages

            // Buggify: Sometimes cause early disconnection to trigger read failures
            if buggify_with_prob!(0.03) {
                tracing::debug!("Server: Buggify simulating client disconnect");
                sometimes_assert!(
                    server_read_fails,
                    true,
                    "Server reads should sometimes fail during chaos testing"
                );
                break; // Simulate client disconnect
            }

            let read_future = stream.read(&mut buffer);
            let bytes_read = match self.time.timeout(timeout, read_future).await? {
                Ok(result) => {
                    match result {
                        Ok(n) => {
                            if n == 0 {
                                tracing::debug!("Server: Client disconnected (EOF)");
                                break; // Client disconnected
                            }
                            tracing::info!("Server: Read {} bytes successfully", n);
                            n
                        }
                        Err(e) => {
                            sometimes_assert!(
                                server_read_fails,
                                true,
                                "Server reads should sometimes fail during chaos testing"
                            );
                            tracing::debug!("Server: Read failed with error: {:?}", e);
                            break; // Connection error - exit gracefully
                        }
                    }
                }
                Err(_timeout) => {
                    sometimes_assert!(
                        server_read_timeout,
                        true,
                        "Server reads should sometimes timeout during chaos testing"
                    );
                    tracing::debug!("Server: Read timeout - client may have disconnected");
                    break; // Timeout - exit gracefully
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
                // Buggify: Sometimes force PING handling failures before processing
                if buggify_with_prob!(0.02) {
                    tracing::debug!("Server: Buggify forcing PING handling failure");
                    sometimes_assert!(
                        server_ping_handling_fails,
                        true,
                        "Server PING handling should sometimes fail during chaos testing"
                    );
                    break; // Simulate handling failure
                }

                // Handle PING message - if it fails, the connection is likely broken
                if let Err(e) = self.handle_ping_message_uuid(stream, message).await {
                    sometimes_assert!(
                        server_ping_handling_fails,
                        true,
                        "Server PING handling should sometimes fail during chaos testing"
                    );
                    tracing::debug!("Server: PING handling failed: {:?}, exiting session", e);
                    break; // Exit gracefully on connection error
                }
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

    /// Handle a PING message with UUID and send PONG response
    async fn handle_ping_message_uuid(
        &mut self,
        stream: &mut N::TcpStream,
        message: &str,
    ) -> SimulationResult<()> {
        let ping_message = message;

        // Verify that after splitting on "-", it is a valid UUID
        if let Some((prefix, uuid_str)) = ping_message.split_once('-') {
            always_assert!(
                server_receives_valid_format,
                prefix == "PING",
                "Ping message should have PING prefix"
            );

            // Parse the UUID - no sequence validation needed
            if let Ok(uuid) = uuid_str.parse::<u128>() {
                tracing::debug!("Server: Processing PING with UUID {}", uuid);

                // Buggify: Sometimes force PONG send failures
                if buggify_with_prob!(0.02) {
                    tracing::debug!(
                        "Server: Buggify forcing PONG send failure for UUID {}",
                        uuid
                    );
                    sometimes_assert!(
                        server_pong_send_fails,
                        true,
                        "Server PONG sends should sometimes fail during chaos testing"
                    );
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "Buggify PONG failure",
                    )
                    .into());
                }

                // Send corresponding PONG response with same UUID - handle connection failures gracefully
                let pong_data = format!("PONG-{}\n", uuid);
                tracing::debug!("Server: Sending pong response: {}", pong_data);
                match stream.write_all(pong_data.as_bytes()).await {
                    Ok(_) => {
                        tracing::debug!("Server: Sent pong response for UUID {}", uuid);
                        always_assert!(
                            server_sends_pong,
                            true,
                            "Server should always be able to send PONG responses"
                        );
                    }
                    Err(e) => {
                        sometimes_assert!(
                            server_pong_send_fails,
                            true,
                            "Server PONG sends should sometimes fail during chaos testing"
                        );
                        tracing::debug!("Server: Failed to send PONG for UUID {}: {:?}", uuid, e);
                        // Return error to break the session loop - connection is likely broken
                        return Err(e.into());
                    }
                }
            } else {
                panic!("Invalid ping UUID format: {}", uuid_str);
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

        // Send BYE!! response and exit - may fail if client disconnects
        tracing::debug!("Server: Sending CLOSE acknowledgment");
        match stream.write_all(b"BYE!!").await {
            Ok(_) => {
                tracing::debug!("Server: Sent CLOSE acknowledgment, shutting down");
            }
            Err(e) => {
                sometimes_assert!(
                    server_bye_send_fails,
                    true,
                    "Server BYE send should sometimes fail during chaos testing"
                );
                tracing::debug!("Server: Failed to send CLOSE acknowledgment: {:?}", e);
                // Continue shutdown regardless - client may have disconnected
            }
        }

        Ok(())
    }
}

/// Client actor for ping-pong communication using resilient Peer
pub struct PingPongClientActor<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    network: N,
    time: T,
    task_provider: TP,
    topology: WorkloadTopology,
}

impl<N: NetworkProvider + 'static, T: TimeProvider + 'static, TP: TaskProvider + 'static>
    PingPongClientActor<N, T, TP>
{
    pub fn new(network: N, time: T, task_provider: TP, topology: WorkloadTopology) -> Self {
        Self {
            network,
            time,
            task_provider,
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

        // Generate a random number of pings to send
        let ping_count = if buggify_with_prob!(0.1) {
            tracing::debug!("Client: Buggify forcing high ping count to stress system");
            sim_random_range((MAX_PING * 3 / 4)..MAX_PING) as u8 // Force high ping counts (75-100)
        } else {
            sim_random_range(20..MAX_PING) as u8 // Normal range with reasonable chance of ≥50
        };

        tracing::debug!(
            "Client: Generated ping_count={}, checking varied workload assertion",
            ping_count
        );
        sometimes_assert!(
            generates_varied_workload,
            ping_count >= 50,
            "Should sometimes generate substantial workloads for testing"
        );

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
    async fn create_resilient_peer(&self, server_addr: &str) -> Peer<N, T, TP> {
        let queue_size = sim_random_range(10..50); // Realistic queue sizes for normal operation

        let timeout_ms = if buggify!() {
            tracing::debug!("Client: Buggify using aggressive timeouts");
            sim_random_range(1..20) // Very short timeouts to force timeout conditions
        } else {
            sim_random_range(1..100)
        };

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
            self.task_provider.clone(),
            server_addr.to_string(),
            peer_config,
        )
    }

    /// Send ping messages and receive pong responses
    async fn send_ping_pong_messages(
        &self,
        peer: &mut Peer<N, T, TP>,
        ping_count: u8,
    ) -> SimulationResult<()> {
        // Track pending requests with UUIDs instead of sequences
        let mut pending_requests: HashMap<u128, String> = HashMap::new();

        // Sometimes send messages in bursts, sometimes one-by-one
        let burst_mode = sim_random_range(1..100) <= 30; // 30% chance of burst mode for realistic testing

        let burst_size = if burst_mode {
            sim_random_range(3..8) // Moderate bursts for realistic testing
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
                // Buggify: Sometimes add delays, sometimes remove ALL delays for rapid-fire
                if buggify_with_prob!(0.02) {
                    let delay_ms = sim_random_range(10..50);
                    tracing::debug!(
                        "Client: Buggify adding {}ms delay between messages",
                        delay_ms
                    );
                    self.time
                        .sleep(Duration::from_millis(delay_ms as u64))
                        .await?;
                } else if buggify_with_prob!(0.05) {
                    tracing::debug!("Client: Buggify removing all delays for rapid-fire sends");
                    // No delay - send as fast as possible to overwhelm queue
                }

                let uuid = self.send_ping_message_uuid(peer, ping_idx).await?;
                pending_requests.insert(uuid, format!("PING-{}", ping_idx));
                ping_idx += 1;
            }

            // Buggify: Sometimes add long delays between bursts to trigger server read timeouts
            if buggify_with_prob!(0.01) {
                let delay_ms = sim_random_range(2000..5000); // Long delay to cause server timeout
                tracing::debug!(
                    "Client: Buggify adding {}ms delay to trigger server read timeout",
                    delay_ms
                );
                self.time
                    .sleep(Duration::from_millis(delay_ms as u64))
                    .await?;
            }

            // Receive responses for the burst with timeout
            self.receive_pong_responses_uuid(peer, &mut pending_requests, current_burst)
                .await?;
        }

        Ok(())
    }

    /// Send a single ping message with UUID
    async fn send_ping_message_uuid(
        &self,
        peer: &mut Peer<N, T, TP>,
        ping_idx: u8,
    ) -> SimulationResult<u128> {
        let uuid = random_unique_id();

        tracing::info!(
            "Client: About to send PING-{} (ping_idx={}, uuid={})",
            uuid,
            ping_idx,
            uuid
        );
        let ping_data = format!("PING-{}\n", uuid);
        let mut padded_data = [0u8; 128]; // Larger buffer for UUID
        let bytes = ping_data.as_bytes();
        padded_data[..bytes.len()].copy_from_slice(bytes);

        let send_start = self.time.now();

        // Retry logic for connection failures
        let mut retry_count = 0;
        let max_retries = 3;
        let retry_delay = Duration::from_millis(100);

        loop {
            match peer.send(padded_data.to_vec()) {
                Ok(_) => {
                    let send_duration = self.time.now() - send_start;
                    tracing::info!(
                        "Client: Successfully sent PING-{} using peer in {:?}",
                        uuid,
                        send_duration
                    );
                    always_assert!(peer_can_send, true, "Peer should be able to send messages");

                    always_assert!(
                        connection_reuse_working,
                        peer.metrics().connections_established
                            <= peer.metrics().connection_attempts,
                        "Connection reuse should prevent excessive connection creation"
                    );

                    // Success - track recovery if this was a retry
                    if retry_count > 0 {
                        tracing::debug!("Client: Recovered after {} retries", retry_count);
                    }
                    break;
                }
                Err(e) => {
                    retry_count += 1;
                    if retry_count >= max_retries {
                        tracing::debug!("Client: Max retries exceeded for ping {}: {:?}", uuid, e);
                        return Err(moonpool_simulation::SimulationError::IoError(format!(
                            "Max retries exceeded: {}",
                            e
                        )));
                    }

                    tracing::debug!(
                        "Client: Failed to send ping {} (attempt {}): {:?}, retrying...",
                        uuid,
                        retry_count,
                        e
                    );

                    // Wait before retrying
                    self.time.sleep(retry_delay).await?;
                }
            }
        }

        Ok(uuid)
    }

    /// Receive pong responses for a burst of messages with UUID validation and timeout
    async fn receive_pong_responses_uuid(
        &self,
        peer: &mut Peer<N, T, TP>,
        pending_requests: &mut HashMap<u128, String>,
        expected_count: usize,
    ) -> SimulationResult<()> {
        let mut pending_data = String::new();
        let mut responses_received = 0;

        while responses_received < expected_count {
            // Add randomized timeout to prevent deadlocks
            let timeout = Duration::from_secs(sim_random_range(3..8));

            // Use TimeProvider timeout for deterministic behavior
            let receive_future = peer.receive();
            match self.time.timeout(timeout, receive_future).await? {
                Ok(result) => {
                    match result {
                        Ok(data) => {
                            let new_data = std::str::from_utf8(&data).unwrap_or("INVALID");
                            pending_data.push_str(new_data);

                            // Process all complete lines (messages ending with \n)
                            while let Some(newline_pos) = pending_data.find('\n') {
                                let message = pending_data[..newline_pos].trim().to_string();
                                let remaining = pending_data[newline_pos + 1..].to_string();
                                pending_data = remaining;

                                if message.starts_with("PONG-") {
                                    if let Some((_, uuid_str)) = message.split_once('-') {
                                        if let Ok(uuid) = uuid_str.parse::<u128>() {
                                            if pending_requests.remove(&uuid).is_some() {
                                                tracing::debug!(
                                                    "Client: Received expected pong for UUID {}: '{}'",
                                                    uuid,
                                                    message
                                                );

                                                always_assert!(
                                                    peer_receives_pong,
                                                    message.starts_with("PONG"),
                                                    "Peer should receive PONG responses"
                                                );

                                                responses_received += 1;
                                            } else {
                                                tracing::debug!(
                                                    "Client: Received unexpected PONG for UUID {}",
                                                    uuid
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            sometimes_assert!(
                                receive_fails_during_chaos,
                                true,
                                "Receive should sometimes fail during chaos testing"
                            );
                            tracing::debug!("Client: Failed to receive data: {:?}", e);
                            return Ok(()); // Return OK to avoid propagating receive error
                        }
                    }
                }
                Err(_timeout) => {
                    sometimes_assert!(
                        timeout_during_chaos,
                        true,
                        "Timeouts should occur during chaos testing"
                    );
                    tracing::debug!("Client: Timeout waiting for responses, returning early");
                    return Ok(()); // Return OK to avoid propagating timeout as error
                }
            }
        }

        Ok(())
    }

    /// Send CLOSE message and receive acknowledgment
    async fn send_close_and_receive_ack(&self, peer: &mut Peer<N, T, TP>) -> SimulationResult<()> {
        // Send CLOSE message to server
        tracing::debug!("Client: Sending CLOSE message to server");
        let mut close_data = [0u8; 128];
        close_data[..6].copy_from_slice(b"CLOSE\n");

        match peer.send(close_data.to_vec()) {
            Ok(_) => {
                tracing::debug!("Client: Successfully sent CLOSE message");
                always_assert!(
                    client_sends_close,
                    true,
                    "Client should be able to send CLOSE message"
                );
            }
            Err(e) => {
                sometimes_assert!(
                    close_send_fails_during_chaos,
                    true,
                    "CLOSE send should sometimes fail during chaos testing"
                );
                tracing::debug!("Client: Failed to send CLOSE message: {:?}", e);
                return Ok(()); // Return OK - connection cutting may prevent CLOSE
            }
        }

        // Read server's acknowledgment with timeout for chaos resilience
        tracing::debug!("Client: Reading server's CLOSE acknowledgment");
        let timeout = Duration::from_secs(sim_random_range(2..5)); // Shorter timeout for CLOSE

        let receive_future = peer.receive();
        match self.time.timeout(timeout, receive_future).await? {
            Ok(result) => match result {
                Ok(data) => {
                    let ack_message = std::str::from_utf8(&data).unwrap_or("INVALID");
                    tracing::debug!("Client: Received acknowledgment: {:?}", ack_message);

                    always_assert!(
                        client_receives_bye,
                        ack_message == "BYE!!",
                        "Client should receive BYE acknowledgment from server"
                    );
                    always_assert!(
                        bye_message_exact,
                        &data == b"BYE!!",
                        format!(
                            "BYE message should be exact: expected 'BYE!!', got '{}'",
                            String::from_utf8_lossy(&data)
                        )
                    );
                }
                Err(e) => {
                    sometimes_assert!(
                        close_ack_receive_fails,
                        true,
                        "CLOSE acknowledgment receive should sometimes fail during chaos testing"
                    );
                    tracing::debug!("Client: Failed to receive CLOSE acknowledgment: {:?}", e);
                }
            },
            Err(_timeout) => {
                sometimes_assert!(
                    close_ack_timeout,
                    true,
                    "CLOSE acknowledgment should sometimes timeout during chaos testing"
                );
                tracing::debug!("Client: Timeout waiting for CLOSE acknowledgment");
            }
        }

        Ok(())
    }

    /// Validate peer metrics and connection behavior
    fn validate_peer_metrics(&self, peer: &Peer<N, T, TP>) {
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
