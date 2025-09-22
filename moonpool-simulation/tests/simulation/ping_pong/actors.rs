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
> {
    transport: ClientTransport<N, T, TP, RequestResponseSerializer>,
    server_address: String,
    topology: WorkloadTopology,
    messages_sent: usize,
}

impl<
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
> PingPongClientActor<N, T, TP>
{
    pub fn new(network: N, time: T, task_provider: TP, topology: WorkloadTopology) -> Self {
        let serializer = RequestResponseSerializer::new();
        let peer_config = PeerConfig::default();
        let transport = ClientTransport::new(serializer, network, time, task_provider, peer_config);

        // Get server address from peer_ips (assuming single server for now)
        let server_address = topology
            .peer_ips
            .first()
            .map(|ip| ip.clone())
            .unwrap_or_else(|| "127.0.0.1:8080".to_string());

        Self {
            transport,
            server_address,
            topology,
            messages_sent: 0,
        }
    }

    /// Main client loop using FoundationDB-inspired tokio::select! pattern
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        tracing::debug!("Client: Starting, connecting to {}", self.server_address);

        while self.messages_sent < MAX_PING {
            tokio::select! {
                // Handle shutdown signal - prioritize this first
                _ = self.topology.shutdown_signal.cancelled() => {
                    tracing::debug!("Client: Shutdown signal received, sent {} messages", self.messages_sent);
                    return Ok(SimulationMetrics::default());
                }

                // Send ping and wait for pong
                result = self.transport.request::<RequestResponseEnvelopeFactory>(&self.server_address, b"PING".to_vec()) => {
                    match result {
                        Ok(response) if response == b"PONG" => {
                            self.messages_sent += 1;
                            tracing::debug!("Client: Successfully completed ping-pong #{}", self.messages_sent);
                        }
                        Ok(_) => {
                            return Err(SimulationError::IoError("Invalid response".to_string()));
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
