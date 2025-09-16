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
use tokio::select;
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
    pub async fn run(
        &mut self,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> SimulationResult<SimulationMetrics> {
        // Bind to server address
        self.transport.bind(&self.bind_address).await.map_err(|e| {
            moonpool_simulation::SimulationError::IoError(format!("Bind failed: {:?}", e))
        })?;

        tracing::debug!("Server: Bound to {}", self.bind_address);

        // Handle incoming messages with tokio::select! for proper shutdown
        loop {
            select! {
                // Wait for shutdown signal
                shutdown_result = &mut shutdown_rx => {
                    match shutdown_result {
                        Ok(()) => {
                            tracing::debug!("Server: Received shutdown signal, closing transport");
                            self.transport.close().await;
                            tracing::debug!("Server: Shutdown complete");
                            return Ok(SimulationMetrics::default());
                        }
                        Err(_) => {
                            tracing::debug!("Server: Shutdown channel closed");
                            return Ok(SimulationMetrics::default());
                        }
                    }
                }
                // Do transport work
                _ = self.do_transport_work() => {
                    // Continue loop for next iteration
                }
            }
        }
    }

    async fn do_transport_work(&mut self) -> SimulationResult<()> {
        // Do transport work
        self.transport.tick().await;
        tracing::trace!("Server: Completed tick, polling for messages");

        while let Some(received) = self.transport.poll_receive() {
            let message = String::from_utf8_lossy(&received.envelope.payload);
            tracing::debug!("Server: Received message: '{}'", message);

            match message.as_ref() {
                "PING" => {
                    self.handle_ping(&received.from, &received.envelope).await?;
                }
                _ => {
                    tracing::warn!("Server: Unknown message: '{}'", message);
                }
            }
        }

        tracing::trace!("Server: No messages received, yielding");
        tokio::task::yield_now().await;
        Ok(())
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
}

// Legacy actor wrappers for compatibility with existing test infrastructure

/// Server actor wrapper for existing test infrastructure
pub struct PingPongServerActor<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    server: Server<N, T, TP>,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
}

impl<N: NetworkProvider + 'static, T: TimeProvider + 'static, TP: TaskProvider + 'static>
    PingPongServerActor<N, T, TP>
{
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
        topology: WorkloadTopology,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        let server = Server::new(network, time, task_provider, topology.my_ip);
        Self {
            server,
            shutdown_rx,
        }
    }

    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        // Take the shutdown_rx to pass it to the server
        let shutdown_rx = std::mem::replace(&mut self.shutdown_rx, {
            let (tx, rx) = tokio::sync::oneshot::channel();
            drop(tx); // Drop the sender immediately so receiver is closed
            rx
        });
        self.server.run(shutdown_rx).await
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
