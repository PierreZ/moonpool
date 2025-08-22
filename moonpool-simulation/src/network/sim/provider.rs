use super::stream::{SimTcpListener, SimTcpStream};
use crate::network::traits::NetworkProvider;
use crate::{Event, WeakSimWorld};
use async_trait::async_trait;
use std::io;
use tracing::instrument;

/// Simulated networking implementation
#[derive(Debug, Clone)]
pub struct SimNetworkProvider {
    sim: WeakSimWorld,
}

impl SimNetworkProvider {
    /// Create a new simulated network provider
    pub fn new(sim: WeakSimWorld) -> Self {
        Self { sim }
    }

    /// Sleep in simulation time.
    ///
    /// This allows workloads to introduce delays for coordination purposes.
    /// The sleep completes when the simulation processes the corresponding Wake event.
    pub fn sleep(
        &self,
        duration: std::time::Duration,
    ) -> crate::SimulationResult<crate::SleepFuture> {
        self.sim.sleep(duration)
    }
}

#[async_trait(?Send)]
impl NetworkProvider for SimNetworkProvider {
    type TcpStream = SimTcpStream;
    type TcpListener = SimTcpListener;

    #[instrument(skip(self))]
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Get bind delay from network configuration and schedule bind completion event
        let delay = sim.with_network_config(|config| config.latency.bind_latency.sample());

        // Schedule bind completion event to advance simulation time
        let listener_id = sim
            .create_listener(addr.to_string())
            .map_err(|e| io::Error::other(format!("Failed to create listener: {}", e)))?;

        // Schedule an event to simulate the bind delay - this advances simulation time
        sim.schedule_event(
            Event::BindComplete {
                listener_id: listener_id.0,
            },
            delay,
        );

        let listener = SimTcpListener::new(self.sim.clone(), listener_id, addr.to_string());
        Ok(listener)
    }

    #[instrument(skip(self))]
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Get connect delay from network configuration and schedule connection event
        let delay = sim.with_network_config(|config| config.latency.connect_latency.sample());

        // Create a connection pair for bidirectional communication
        let (client_id, server_id) = sim
            .create_connection_pair("client-addr".to_string(), addr.to_string())
            .map_err(|e| io::Error::other(format!("Failed to create connection pair: {}", e)))?;

        // Store the server side for accept() to pick up later
        sim.store_pending_connection(addr, server_id)
            .map_err(|e| io::Error::other(format!("Failed to store pending connection: {}", e)))?;

        // Schedule connection ready event to advance simulation time
        sim.schedule_event(
            Event::ConnectionReady {
                connection_id: client_id.0,
            },
            delay,
        );

        let stream = SimTcpStream::new(self.sim.clone(), client_id);
        Ok(stream)
    }
}
