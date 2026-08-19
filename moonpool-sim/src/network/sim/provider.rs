use super::stream::{SimTcpListener, SimTcpStream};
use crate::NetworkProvider;
use crate::buggify;
use crate::network::ConnectFailureMode;
use crate::sim::rng::sim_random;
use crate::{Event, WeakSimWorld};
use std::io;
use std::net::IpAddr;
use tracing::instrument;

/// Simulated networking implementation
///
/// Scoped to the IP of the process that owns it: [`connect`](Self::connect)
/// uses this IP as the local address for the connections it initiates, so
/// per-pair chaos (e.g. `max_pair_latency`) can key off the real client IP
/// instead of a placeholder.
#[derive(Debug, Clone)]
pub struct SimNetworkProvider {
    sim: WeakSimWorld,
    /// IP address of the process that owns connections initiated through
    /// this provider.
    local_ip: IpAddr,
}

impl SimNetworkProvider {
    /// Create a new simulated network provider scoped to a process IP.
    #[must_use]
    pub fn new(sim: WeakSimWorld, local_ip: IpAddr) -> Self {
        Self { sim, local_ip }
    }

    /// Sleep in simulation time.
    ///
    /// This allows workloads to introduce delays for coordination purposes.
    /// The sleep completes when the simulation processes the corresponding Wake event.
    ///
    /// # Errors
    ///
    /// Returns an error if the simulation cannot schedule the sleep event.
    pub fn sleep(
        &self,
        duration: std::time::Duration,
    ) -> crate::SimulationResult<crate::SleepFuture> {
        self.sim.sleep(duration)
    }
}

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
        let delay =
            sim.with_network_config(|config| crate::network::sample_latency(&config.bind_latency));

        // Schedule bind completion event to advance simulation time
        let listener_id = sim.create_listener();

        // Schedule an event to simulate the bind delay - this advances simulation time
        sim.schedule_event(
            Event::Connection {
                id: listener_id.0,
                state: crate::ConnectionStateChange::BindComplete,
            },
            delay,
        );

        let listener = SimTcpListener::new(self.sim.clone(), addr.to_string());
        Ok(listener)
    }

    /// Connect to a remote address.
    ///
    /// When chaos is enabled, connection establishment can fail or hang forever
    /// based on the `connect_failure_mode` setting (FDB ref: sim2.actor.cpp:1243-1250):
    /// - Disabled: Normal operation (no failure injection)
    /// - `AlwaysFail`: Always fail with `ConnectionRefused` when buggified
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
                    }
                    // Hang forever - create a future that never completes
                    // This tests timeout handling in connection retry logic
                    tracing::debug!(addr = %addr, "Connection hanging forever (Probabilistic mode - hang)");
                    std::future::pending::<()>().await;
                    unreachable!("pending() never resolves");
                }
            }
        }

        // Get connect delay from network configuration and schedule connection event
        let delay = sim
            .with_network_config(|config| crate::network::sample_latency(&config.connect_latency));

        // Create a connection pair for bidirectional communication, using this
        // process's real IP as the client's local address (port is irrelevant,
        // only the IP is parsed out) so per-pair chaos can key off it.
        let client_addr = std::net::SocketAddr::new(self.local_ip, 0).to_string();
        let (client_id, server_id) = sim.create_connection_pair(&client_addr, addr);

        // FDB SimClogging: fix a permanent per-pair latency at first contact, for
        // both directions. No-op (returns ZERO, no RNG) when max_pair_latency is off.
        let _ = sim.connection_base_latency(client_id);
        let _ = sim.connection_base_latency(server_id);

        // Store the server side for accept() to pick up later
        sim.store_pending_connection(addr, server_id);

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
