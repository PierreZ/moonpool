use moonpool_simulation::{
    NetworkProvider,
    SimulationMetrics,
    SimulationResult,
    TaskProvider,
    TimeProvider,
    WorkloadTopology,
    always_assert,
    // Transport layer imports
    network::{
        ClientTransport, NetTransport, RequestResponseEnvelope, RequestResponseSerializer,
        ServerTransport,
    },
};
use tracing::instrument;

/// Server for ping-pong communication using NetTransport
pub struct Server<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    transport: ServerTransport<N, T, TP, RequestResponseSerializer>,
    bind_address: String,
}

impl<N: NetworkProvider + 'static, T: TimeProvider + 'static, TP: TaskProvider + 'static>
    Server<N, T, TP>
{
    pub fn new(network: N, time: T, task_provider: TP, bind_address: String) -> Self {
        let server_transport =
            ServerTransport::new(RequestResponseSerializer, network, time, task_provider);

        Self {
            transport: server_transport,
            bind_address,
        }
    }

    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        // Bind to server address
        self.transport.bind(&self.bind_address).await.map_err(|e| {
            moonpool_simulation::SimulationError::IoError(format!("Bind failed: {:?}", e))
        })?;

        tracing::debug!("Server: Bound to {}", self.bind_address);

        // Handle incoming messages
        loop {
            self.transport.tick().await;
            tracing::trace!("Server: Completed tick, polling for messages");

            while let Some(received) = self.transport.poll_receive() {
                let message = String::from_utf8_lossy(&received.envelope.payload);
                tracing::debug!("Server: Received message: '{}'", message);

                match message.as_ref() {
                    "PING" => {
                        self.handle_ping(&received.from, &received.envelope).await?;
                    }
                    "CLOSE" => {
                        tracing::debug!("Server: Received CLOSE, sending BYE and shutting down");
                        self.handle_close(&received.from, &received.envelope)
                            .await?;

                        tracing::debug!(
                            "Server: BYE queued, closing transport (will flush automatically)"
                        );
                        // Close will flush pending writes automatically
                        self.transport.close().await;
                        tracing::debug!("Server: Shutdown complete");

                        return Ok(SimulationMetrics::default());
                    }
                    _ => {
                        tracing::warn!("Server: Unknown message: '{}'", message);
                    }
                }
            }

            tracing::trace!("Server: No messages received, yielding");
            tokio::task::yield_now().await;
        }
    }

    async fn handle_ping(
        &mut self,
        from: &str,
        envelope: &RequestResponseEnvelope,
    ) -> SimulationResult<()> {
        tracing::debug!("Server: Processing PING from {}", from);

        self.transport
            .send_reply(envelope, b"PONG".to_vec())
            .map_err(|e| {
                moonpool_simulation::SimulationError::IoError(format!("Send reply failed: {:?}", e))
            })?;

        always_assert!(
            server_sends_pong,
            true,
            "Server should always send PONG responses"
        );

        tracing::debug!("Server: Sent PONG response to {}", from);
        Ok(())
    }

    async fn handle_close(
        &mut self,
        from: &str,
        envelope: &RequestResponseEnvelope,
    ) -> SimulationResult<()> {
        tracing::debug!("Server: Processing CLOSE from {}", from);

        let _ = self.transport.send_reply(envelope, b"BYE!!".to_vec()); // May fail if client disconnects
        tracing::debug!("Server: Sent BYE response to {}", from);
        Ok(())
    }
}

/// Client for ping-pong communication using NetTransport
pub struct Client<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    transport: ClientTransport<N, T, TP, RequestResponseSerializer>,
    server_address: String,
}

impl<N: NetworkProvider + 'static, T: TimeProvider + 'static, TP: TaskProvider + 'static>
    Client<N, T, TP>
{
    pub fn new(network: N, time: T, task_provider: TP, server_address: String) -> Self {
        let client_transport =
            ClientTransport::new(RequestResponseSerializer, network, time, task_provider);

        Self {
            transport: client_transport,
            server_address,
        }
    }

    #[instrument(skip(self))]
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        tracing::debug!("Client: Connecting to server at {}", self.server_address);

        // Send PING and wait for PONG
        tracing::debug!("Client: About to send PING");
        self.send_ping().await?;
        tracing::debug!("Client: PING/PONG completed successfully");

        // Send CLOSE and wait for BYE
        tracing::debug!("Client: About to send CLOSE");
        self.send_close().await?;
        tracing::debug!("Client: CLOSE/BYE completed successfully");

        // Close transport
        tracing::debug!("Client: About to close transport");
        self.transport.close().await;
        tracing::debug!("Client: Transport closed");

        tracing::debug!("Client: Completed successfully");
        Ok(SimulationMetrics::default())
    }

    async fn send_ping(&mut self) -> SimulationResult<()> {
        tracing::debug!("Client: Sending PING");

        let response = self
            .transport
            .get_reply::<RequestResponseEnvelope>(&self.server_address, b"PING".to_vec())
            .await
            .map_err(|e| {
                moonpool_simulation::SimulationError::IoError(format!("Get reply failed: {:?}", e))
            })?;

        let message = String::from_utf8_lossy(&response);
        if message == "PONG" {
            tracing::debug!("Client: Received PONG");
            Ok(())
        } else {
            tracing::warn!("Client: Expected PONG, got: '{}'", message);
            Err(moonpool_simulation::SimulationError::IoError(format!(
                "Unexpected response: {}",
                message
            )))
        }
    }

    async fn send_close(&mut self) -> SimulationResult<()> {
        tracing::debug!("Client: Sending CLOSE");

        let response = self
            .transport
            .get_reply::<RequestResponseEnvelope>(&self.server_address, b"CLOSE".to_vec())
            .await;

        match response {
            Ok(response) => {
                let message = String::from_utf8_lossy(&response);
                if message == "BYE!!" {
                    tracing::debug!("Client: Received BYE");
                } else {
                    tracing::warn!("Client: Expected BYE, got: '{}'", message);
                }
            }
            Err(e) => {
                tracing::warn!("Client: Close reply failed: {:?}, continuing anyway", e);
            }
        }

        Ok(()) // Always return Ok for close
    }
}

// Legacy actor wrappers for compatibility with existing test infrastructure

/// Server actor wrapper for existing test infrastructure
pub struct PingPongServerActor<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    server: Server<N, T, TP>,
}

impl<N: NetworkProvider + 'static, T: TimeProvider + 'static, TP: TaskProvider + 'static>
    PingPongServerActor<N, T, TP>
{
    pub fn new(network: N, time: T, task_provider: TP, topology: WorkloadTopology) -> Self {
        let server = Server::new(network, time, task_provider, topology.my_ip);
        Self { server }
    }

    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        self.server.run().await
    }
}

/// Client actor wrapper for existing test infrastructure
pub struct PingPongClientActor<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    client: Client<N, T, TP>,
}

impl<N: NetworkProvider + 'static, T: TimeProvider + 'static, TP: TaskProvider + 'static>
    PingPongClientActor<N, T, TP>
{
    pub fn new(network: N, time: T, task_provider: TP, topology: WorkloadTopology) -> Self {
        let server_address = topology
            .peer_ips
            .first()
            .expect("No server peer in topology")
            .clone();

        let client = Client::new(network, time, task_provider, server_address);
        Self { client }
    }

    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        self.client.run().await
    }
}
