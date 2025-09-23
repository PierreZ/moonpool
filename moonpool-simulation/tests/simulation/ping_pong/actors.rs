use moonpool_simulation::{
    NetworkProvider, PeerConfig, SimulationError, SimulationMetrics, SimulationResult,
    TaskProvider, TimeProvider, WorkloadTopology,
    network::transport::{
        ClientTransport, Envelope, RequestResponseEnvelopeFactory, RequestResponseSerializer,
        ServerTransport,
    },
};
use tracing::instrument;

static MAX_PING: usize = 100;

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
    transport: ServerTransport<N, T, TP, RequestResponseSerializer>,
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
        let serializer = RequestResponseSerializer::new();
        let transport = ServerTransport::new(network, time, task_provider, serializer);

        Self {
            transport,
            topology,
            messages_handled: 0,
        }
    }

    /// Main server loop using FoundationDB-inspired tokio::select! pattern
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        tracing::debug!(
            "Server: Starting on {} for {} client(s)",
            self.topology.my_ip,
            self.topology.peer_ips.len()
        );

        // Bind to server address - fail immediately if this doesn't work
        if let Err(e) = self.transport.bind(&self.topology.my_ip).await {
            tracing::error!("Server: Failed to bind to {}: {}", self.topology.my_ip, e);
            return Err(SimulationError::IoError(format!(
                "Server bind failed: {}",
                e
            )));
        }

        tracing::debug!("Server: Successfully bound and ready for connections");

        // FoundationDB-style single select loop handling all events
        loop {
            tokio::select! {
                // Clean shutdown - prioritize this branch first
                _ = self.topology.shutdown_signal.cancelled() => {
                    tracing::debug!("Server: Shutdown signal received, handled {} messages", self.messages_handled);
                    self.transport.close().await;
                    return Ok(SimulationMetrics::default());
                }

                // Handle incoming messages OR transport errors
                result = self.transport.try_next_message() => {
                    match result {
                        Ok(Some(msg)) if msg.envelope.payload() == b"PING" => {
                            self.messages_handled += 1;
                            tracing::debug!("Server: Handling ping #{}", self.messages_handled);

                            // Send PONG response
                            if let Err(e) = self.transport.send_reply::<RequestResponseEnvelopeFactory>(
                                &msg.envelope,
                                b"PONG".to_vec(),
                            ) {
                                tracing::warn!("Server: Failed to send PONG: {}", e);
                            }
                        }
                        Ok(Some(_)) => {
                            // Ignore non-PING messages
                            tracing::debug!("Server: Received non-PING message, ignoring");
                        }
                        Ok(None) => {
                            // No message ready - yield and continue
                            tokio::task::yield_now().await;
                        }
                        Err(e) => {
                            // Transport error - connection lost, read failed, etc.
                            tracing::warn!("Server: Transport error: {}", e);
                            // Continue for next connection
                        }
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
    R: moonpool_simulation::random::RandomProvider,
> {
    transport: ClientTransport<N, T, TP, RequestResponseSerializer>,
    server_addresses: Vec<String>,
    topology: WorkloadTopology,
    messages_sent: usize,
    random: R,
    selection_strategy: SelectionStrategy,
    current_server_index: usize,
}

impl<
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    R: moonpool_simulation::random::RandomProvider,
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
        }
    }

    /// Select the next server address based on the chosen strategy
    fn select_next_server(&mut self) -> &str {
        match self.selection_strategy {
            SelectionStrategy::RoundRobin => {
                let server = &self.server_addresses[self.current_server_index];
                self.current_server_index =
                    (self.current_server_index + 1) % self.server_addresses.len();
                server
            }
            SelectionStrategy::Random => {
                let index = self.random.random_range(0..self.server_addresses.len());
                &self.server_addresses[index]
            }
        }
    }

    /// Main client loop using FoundationDB-inspired tokio::select! pattern
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        tracing::debug!(
            "Client: Starting with {} servers using {:?} strategy",
            self.server_addresses.len(),
            self.selection_strategy
        );

        while self.messages_sent < MAX_PING {
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
                    return Ok(SimulationMetrics::default());
                }

                // Send ping with random timeout to create back-pressure
                result = self.transport.request_with_timeout::<RequestResponseEnvelopeFactory>(
                    &selected_server,
                    b"PING".to_vec(),
                    std::time::Duration::from_millis(timeout_ms)
                ) => {
                    match result {
                        Ok(response) if response == b"PONG" => {
                            self.messages_sent += 1;
                            tracing::debug!("Client: Successfully completed ping-pong #{}", self.messages_sent);
                        }
                        Ok(_) => {
                            return Err(SimulationError::IoError("Invalid response".to_string()));
                        }
                        Err(moonpool_simulation::network::transport::TransportError::Timeout) => {
                            tracing::warn!("Client: Request timed out, counting as failed attempt");
                            // Still count this as a message attempt to avoid infinite loops
                            self.messages_sent += 1;
                        }
                        Err(e) => {
                            tracing::warn!("Client: Ping failed: {}", e);
                            // Continue trying - transport will handle reconnection
                        }
                    }
                }
            }
        }

        tracing::debug!(
            "Client: Completed {} ping-pong exchanges",
            self.messages_sent
        );
        self.transport.close().await;
        Ok(SimulationMetrics::default())
    }
}
