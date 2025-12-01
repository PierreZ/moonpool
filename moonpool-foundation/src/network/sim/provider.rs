use super::stream::{SimTcpListener, SimTcpStream};
use crate::buggify;
use crate::network::config::ConnectFailureMode;
use crate::network::traits::NetworkProvider;
use crate::sim::rng::sim_random;
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
        let delay = sim.with_network_config(|config| {
            crate::network::config::sample_duration(&config.bind_latency)
        });

        // Schedule bind completion event to advance simulation time
        let listener_id = sim
            .create_listener(addr.to_string())
            .map_err(|e| io::Error::other(format!("Failed to create listener: {}", e)))?;

        // Schedule an event to simulate the bind delay - this advances simulation time
        sim.schedule_event(
            Event::Connection {
                id: listener_id.0,
                state: crate::ConnectionStateChange::BindComplete,
            },
            delay,
        );

        let listener = SimTcpListener::new(self.sim.clone(), listener_id, addr.to_string());
        Ok(listener)
    }

    /// Connect to a remote address.
    ///
    /// When chaos is enabled, connection establishment can fail or hang forever
    /// based on the connect_failure_mode setting (FDB ref: sim2.actor.cpp:1243-1250):
    /// - Disabled: Normal operation (no failure injection)
    /// - AlwaysFail: Always fail with ConnectionRefused when buggified
    /// - Probabilistic: 50% fail with error, 50% hang forever (tests timeout handling)
    #[instrument(skip(self))]
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Check connect failure mode (FDB SIM_CONNECT_ERROR_MODE pattern)
        // FDB ref: sim2.actor.cpp:1243-1250
        let (failure_mode, failure_probability) = sim.with_network_config(|config| {
            (
                config.chaos.connect_failure_mode,
                config.chaos.connect_failure_probability,
            )
        });

        match failure_mode {
            ConnectFailureMode::Disabled => {} // Normal operation
            ConnectFailureMode::AlwaysFail => {
                // Always fail with connection_failed when buggified
                if buggify!() {
                    tracing::debug!(addr = %addr, "Connection establishment failed (AlwaysFail mode)");
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "Connection establishment failed (AlwaysFail mode)",
                    ));
                }
            }
            ConnectFailureMode::Probabilistic => {
                // Probabilistic - fail or hang forever
                if buggify!() {
                    if sim_random::<f64>() > failure_probability {
                        // Throw connection_failed error
                        tracing::debug!(addr = %addr, "Connection establishment failed (Probabilistic mode - error)");
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            "Connection establishment failed (Probabilistic mode)",
                        ));
                    } else {
                        // Hang forever - create a future that never completes
                        // This tests timeout handling in connection retry logic
                        tracing::debug!(addr = %addr, "Connection hanging forever (Probabilistic mode - hang)");
                        std::future::pending::<()>().await;
                        unreachable!("pending() never resolves");
                    }
                }
            }
        }

        // Get connect delay from network configuration and schedule connection event
        let delay = sim.with_network_config(|config| {
            crate::network::config::sample_duration(&config.connect_latency)
        });

        // Create a connection pair for bidirectional communication
        let (client_id, server_id) = sim
            .create_connection_pair("client-addr".to_string(), addr.to_string())
            .map_err(|e| io::Error::other(format!("Failed to create connection pair: {}", e)))?;

        // Store the server side for accept() to pick up later
        sim.store_pending_connection(addr, server_id)
            .map_err(|e| io::Error::other(format!("Failed to store pending connection: {}", e)))?;

        // Schedule connection ready event to advance simulation time
        sim.schedule_event(
            Event::Connection {
                id: client_id.0,
                state: crate::ConnectionStateChange::ConnectionReady,
            },
            delay,
        );

        let stream = SimTcpStream::new(self.sim.clone(), client_id);
        Ok(stream)
    }
}
