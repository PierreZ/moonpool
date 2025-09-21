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

    /// Main server loop using transport layer
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        let server_addr = self.topology.my_ip.clone();
        tracing::debug!(
            "Server: Starting transport-based server on {} for {} client(s): {:?}",
            self.topology.my_ip,
            self.topology.peer_ips.len(),
            self.topology.peer_ips
        );

        // Bind to server address using transport
        tracing::debug!("Server: Attempting to bind to {}", server_addr);
        self.transport
            .bind(&server_addr)
            .await
            .map_err(|e| SimulationError::IoError(format!("Server bind failed: {}", e)))?;

        tracing::warn!(
            "Server: Successfully bound to {} and ready for connections",
            server_addr
        );

        // Main message handling loop
        loop {
            // Check for shutdown signal
            if self.topology.shutdown_signal.is_cancelled() {
                tracing::debug!(
                    "Server: Shutdown signal received, handled {} messages",
                    self.messages_handled
                );
                return Ok(SimulationMetrics::default());
            }

            // FIRST: Transport maintenance (accept connections, read data)
            tracing::debug!("Server: Calling tick() for transport maintenance");
            let _ = self.transport.tick().await;

            // THEN: Process incoming messages from buffer
            if let Some(received_envelope) = self.transport.poll_receive() {
                tracing::debug!("Server: poll_receive returned a message");
                let payload = received_envelope.envelope.payload();
                let message = String::from_utf8_lossy(payload);

                tracing::debug!("Server: Received message: {}", message);

                if message.trim() == "PING" {
                    self.messages_handled += 1;
                    tracing::debug!("Server: Handling ping #{}", self.messages_handled);

                    // Send PONG response using transport
                    let response = self.transport.send_reply::<RequestResponseEnvelopeFactory>(
                        &received_envelope.envelope,
                        b"PONG".to_vec(),
                    );

                    if let Err(e) = response {
                        tracing::warn!("Server: Failed to send PONG: {}", e);
                    } else {
                        tracing::debug!("Server: Sent PONG response");
                    }

                    // Check completion condition
                    if self.messages_handled >= MAX_PING {
                        tracing::debug!(
                            "Server: Completed {} ping-pong exchanges",
                            self.messages_handled
                        );

                        // Give a moment for the final response to be delivered
                        tracing::debug!("Server: Waiting for final response delivery");
                        for _ in 0..10 {
                            tokio::task::yield_now().await;
                        }

                        // Clean up transport resources
                        self.transport.close().await;
                        tracing::debug!("Server: Transport closed, exiting");

                        return Ok(SimulationMetrics::default());
                    }
                } else {
                    tracing::warn!("Server: Received unexpected message: {}", message);
                }
            } else {
                // No messages available, yield to allow other tasks to run
                tracing::debug!("Server: No messages available, yielding");
                tokio::task::yield_now().await;
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

    /// Main client loop using transport layer
    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        tracing::debug!(
            "Client: Starting transport-based client connecting to {}",
            self.server_address
        );

        // Server should be ready immediately in simulation

        // Send ping-pong messages with failure handling
        let mut consecutive_failures = 0;
        const MAX_CONSECUTIVE_FAILURES: usize = 10;

        while self.messages_sent < MAX_PING {
            // Check for shutdown signal
            if self.topology.shutdown_signal.is_cancelled() {
                tracing::debug!(
                    "Client: Shutdown signal received, sent {} messages",
                    self.messages_sent
                );
                return Ok(SimulationMetrics::default());
            }

            // Send ping with timeout
            match self.send_ping().await {
                Ok(_) => {
                    consecutive_failures = 0; // Reset on success
                    self.messages_sent += 1;
                    tracing::debug!(
                        "Client: Successfully completed ping-pong #{}",
                        self.messages_sent
                    );
                }
                Err(e) => {
                    consecutive_failures += 1;
                    tracing::warn!(
                        "Client: Ping failed (attempt {}): {}",
                        consecutive_failures,
                        e
                    );

                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        return Err(SimulationError::IoError(format!(
                            "Too many consecutive failures: {}",
                            consecutive_failures
                        )));
                    }
                }
            }
        }

        tracing::debug!(
            "Client: Completed {} ping-pong exchanges",
            self.messages_sent
        );

        // Clean up transport resources
        self.transport.close().await;
        tracing::debug!("Client: Transport closed, exiting");

        Ok(SimulationMetrics::default())
    }

    /// Send a single ping and wait for pong response
    async fn send_ping(&mut self) -> SimulationResult<()> {
        tracing::debug!("Client: Sending PING to {}", self.server_address);
        tracing::debug!("Client: About to call transport.get_reply()");

        // Use tokio::select! to race between ping-pong and shutdown signal
        let response = tokio::select! {
            // Try to complete the ping-pong exchange
            result = self.transport.get_reply::<RequestResponseEnvelopeFactory>(&self.server_address, b"PING".to_vec()) => {
                result.map_err(|e| {
                    tracing::debug!("Client: transport.get_reply() failed: {:?}", e);
                    SimulationError::IoError(format!("Transport error: {}", e))
                })?
            }

            // Check for shutdown signal
            _ = self.topology.shutdown_signal.cancelled() => {
                tracing::debug!("Client: Shutdown signal received during ping-pong");
                return Err(SimulationError::IoError("Shutdown signal received".to_string()));
            }
        };

        tracing::debug!("Client: transport.get_reply() succeeded");

        // Verify response
        let response_message = String::from_utf8_lossy(&response);
        if response_message.trim() == "PONG" {
            tracing::debug!("Client: Received expected PONG response");
            Ok(())
        } else {
            Err(SimulationError::IoError(format!(
                "Expected PONG, got: {}",
                response_message
            )))
        }
    }
}
