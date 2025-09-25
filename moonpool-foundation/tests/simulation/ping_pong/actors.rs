use moonpool_foundation::{
    NetworkProvider, PeerConfig, SimulationError, SimulationMetrics, SimulationResult,
    TaskProvider, TimeProvider, WorkloadTopology,
    network::transport::{
        ClientTransport, Envelope, RequestResponseEnvelopeFactory, RequestResponseSerializer,
        ServerTransport,
    },
    sometimes_assert,
};
use tracing::instrument;

#[derive(Debug, Clone)]
enum SelectionStrategy {
    RoundRobin,
    Random,
}

/// Server actor for ping-pong communication using transport layer
pub struct PingPongServerActor<
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
> {
    network: N,
    time: T,
    task_provider: TP,
    topology: WorkloadTopology,
    messages_handled: usize,
}

impl<
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
> PingPongServerActor<N, T, TP>
{
    pub fn new(network: N, time: T, task_provider: TP, topology: WorkloadTopology) -> Self {
        Self {
            network,
            time,
            task_provider,
            topology,
            messages_handled: 0,
        }
    }

    /// Main server loop using event-driven async/await pattern
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        tracing::debug!(
            "Server: Starting on {} for {} client(s)",
            self.topology.my_ip,
            self.topology.peer_ips.len()
        );

        // Create and bind transport - this starts the accept loop automatically
        let mut transport = ServerTransport::bind(
            self.network.clone(),
            self.time.clone(),
            self.task_provider.clone(),
            RequestResponseSerializer::new(),
            &self.topology.my_ip,
        )
        .await
        .map_err(|e| {
            tracing::error!("Server: Failed to bind to {}: {}", self.topology.my_ip, e);
            SimulationError::IoError(format!("Server bind failed: {}", e))
        })?;

        tracing::debug!("Server: Successfully bound and ready for connections");

        // Event-driven loop - no tick() or polling needed!
        loop {
            tokio::select! {
                // Clean shutdown - prioritize this branch first
                _ = self.topology.shutdown_signal.cancelled() => {
                    tracing::debug!(
                        "Server: Shutdown signal received, handled {} messages from {} connections",
                        self.messages_handled,
                        transport.connection_count()
                    );
                    // Transport cleanup is automatic when dropped
                    return Ok(SimulationMetrics::default());
                }

                // Handle incoming messages from ANY connection
                Some(msg) = transport.next_message() => {
                    // Assert when handling multiple connections
                    sometimes_assert!(
                        server_handles_multiple_connections,
                        transport.connection_count() > 1,
                        "Server handling multiple connections"
                    );

                    // Assert when connection is active
                    sometimes_assert!(
                        server_connection_active,
                        true,
                        "Server connection is active"
                    );
                    let payload = String::from_utf8_lossy(msg.envelope.payload());
                    if payload.starts_with("PING:") {
                        let client_ip = payload.strip_prefix("PING:").unwrap_or("unknown");
                        self.messages_handled += 1;

                        // Assert when handling high message rate
                        sometimes_assert!(
                            server_high_message_rate,
                            self.messages_handled > 5,
                            "Server handling high message rate"
                        );

                        tracing::debug!(
                            "Server: Handling ping #{} from client {} (connection {})",
                            self.messages_handled,
                            client_ip,
                            msg.connection_id
                        );

                        // Send PONG response with server IP included
                        let pong_response = format!("PONG:{}", self.topology.my_ip);
                        if let Err(e) = transport.send_reply::<RequestResponseEnvelopeFactory>(
                            &msg.envelope,
                            pong_response.into_bytes(),
                            &msg,
                        ) {
                            tracing::warn!("Server: Failed to send PONG to connection {}: {}", msg.connection_id, e);
                        }
                    } else {
                        // Ignore non-PING messages
                        tracing::debug!(
                            "Server: Received non-PING message from connection {} (payload: {}), ignoring",
                            msg.connection_id,
                            payload
                        );
                    }
                }
            }
        }
    }
}

/// Client actor for ping-pong communication using transport layer
pub struct PingPongClientActor<
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    R: moonpool_foundation::random::RandomProvider,
> {
    transport: ClientTransport<N, T, TP, RequestResponseSerializer>,
    server_addresses: Vec<String>,
    topology: WorkloadTopology,
    messages_sent: usize,
    random: R,
    selection_strategy: SelectionStrategy,
    current_server_index: usize,
    last_selected_server: Option<String>,
}

impl<
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    R: moonpool_foundation::random::RandomProvider,
> PingPongClientActor<N, T, TP, R>
{
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
        topology: WorkloadTopology,
        peer_config: PeerConfig,
        random: R,
    ) -> Self {
        let serializer = RequestResponseSerializer::new();
        let transport = ClientTransport::new(serializer, network, time, task_provider, peer_config);

        // Get all server addresses from peer_ips
        let server_addresses = if topology.peer_ips.is_empty() {
            vec!["127.0.0.1:8080".to_string()]
        } else {
            topology.peer_ips.clone()
        };

        // Randomly choose selection strategy
        let selection_strategy = if random.random_bool(0.5) {
            SelectionStrategy::RoundRobin
        } else {
            SelectionStrategy::Random
        };

        Self {
            transport,
            server_addresses,
            topology,
            messages_sent: 0,
            random,
            selection_strategy,
            current_server_index: 0,
            last_selected_server: None,
        }
    }

    /// Select the next server address based on the chosen strategy
    fn select_next_server(&mut self) -> &str {
        let selected_server = match self.selection_strategy {
            SelectionStrategy::RoundRobin => {
                // Assert when using round-robin strategy
                sometimes_assert!(
                    client_uses_round_robin,
                    true,
                    "Client using round-robin strategy"
                );

                let server = &self.server_addresses[self.current_server_index];
                self.current_server_index =
                    (self.current_server_index + 1) % self.server_addresses.len();
                server
            }
            SelectionStrategy::Random => {
                // Assert when using random strategy
                sometimes_assert!(client_uses_random, true, "Client using random strategy");

                let index = self.random.random_range(0..self.server_addresses.len());
                &self.server_addresses[index]
            }
        };

        // Assert when switching servers
        if let Some(ref last_server) = self.last_selected_server {
            sometimes_assert!(
                client_switches_servers,
                selected_server != last_server,
                "Client switching between servers"
            );
        }

        self.last_selected_server = Some(selected_server.to_string());
        selected_server
    }

    /// Run a single ping operation
    /// Returns Ok(true) if ping was sent, Ok(false) if shutdown signal received, Err on error
    pub async fn run_single_ping(&mut self) -> Result<bool, SimulationError> {
        // Select server based on strategy
        let selected_server = self.select_next_server().to_string();

        // 50% chance of very short timeout to cause backlog
        let timeout_ms = if self.random.random_bool(0.5) {
            self.random.random_range(1..10) // Very short: 1-10ms
        } else {
            self.random.random_range(100..5000) // Normal: 100-5000ms
        };

        tokio::select! {
            // Handle shutdown signal - prioritize this first
            _ = self.topology.shutdown_signal.cancelled() => {
                tracing::debug!("Client: Shutdown signal received, sent {} messages", self.messages_sent);
                return Ok(false);
            }

            // Send ping with source IP included in payload
            result = self.transport.request_with_timeout::<RequestResponseEnvelopeFactory>(
                &selected_server,
                format!("PING:{}", self.topology.my_ip).into_bytes(),
                std::time::Duration::from_millis(timeout_ms)
            ) => {
                match result {
                    Ok(response) => {
                        let response_str = String::from_utf8_lossy(&response);
                        if response_str.starts_with("PONG:") {
                            let server_ip = response_str.strip_prefix("PONG:").unwrap_or("unknown");
                            self.messages_sent += 1;

                            // Assert when completing rapid pings (quick succession)
                            sometimes_assert!(
                                client_rapid_success,
                                self.messages_sent > 3 && timeout_ms < 1000,
                                "Client completing rapid successful pings"
                            );

                            tracing::debug!(
                                "Client: Successfully completed ping-pong #{} with server {}",
                                self.messages_sent,
                                server_ip
                            );
                            Ok(true)
                        } else {
                            tracing::warn!("Client: Unexpected response: {}", response_str);
                            Err(SimulationError::IoError(format!("Invalid response: {}", response_str)))
                        }
                    }
                    Err(moonpool_foundation::network::transport::TransportError::Timeout) => {
                        // Assert when timeout occurs
                        sometimes_assert!(
                            client_timeout_occurred,
                            true,
                            "Client request timeout occurred"
                        );

                        tracing::warn!("Client: Request timed out, counting as failed attempt");
                        // Still count this as a message attempt to avoid infinite loops
                        self.messages_sent += 1;
                        Ok(true)
                    }
                    Err(e) => {
                        // Assert when retrying after failure
                        sometimes_assert!(
                            client_retry_after_failure,
                            true,
                            "Client retrying after failure"
                        );

                        tracing::warn!("Client: Ping failed: {}", e);
                        // Continue trying - transport will handle reconnection
                        Ok(true)
                    }
                }
            }
        }
    }
}
