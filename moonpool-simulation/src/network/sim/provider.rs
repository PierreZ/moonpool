use super::stream::{SimTcpListener, SimTcpStream};
use super::types::{ConnectionId, ListenerId};
use crate::network::traits::{NetworkProvider, TcpListenerTrait};
use crate::{SimulationError, SimulationResult, WeakSimWorld};
use async_trait::async_trait;
use std::io;

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
}

#[async_trait(?Send)]
impl NetworkProvider for SimNetworkProvider {
    type TcpStream = SimTcpStream;
    type TcpListener = SimTcpListener;

    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "simulation shutdown"))?;

        let listener_id = sim.create_listener(addr.to_string()).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to create listener: {}", e),
            )
        })?;

        let listener = SimTcpListener::new(self.sim.clone(), listener_id, addr.to_string());
        Ok(listener)
    }

    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "simulation shutdown"))?;

        let connection_id = sim.create_connection(addr.to_string()).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to create connection: {}", e),
            )
        })?;

        let stream = SimTcpStream::new(self.sim.clone(), connection_id);
        Ok(stream)
    }
}
