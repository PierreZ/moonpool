use super::types::{ConnectionId, ListenerId};
use crate::network::traits::TcpListenerTrait;
use crate::{Event, WeakSimWorld};
use async_trait::async_trait;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Simulated TCP stream
pub struct SimTcpStream {
    sim: WeakSimWorld,
    connection_id: ConnectionId,
}

impl SimTcpStream {
    /// Create a new simulated TCP stream
    pub(crate) fn new(sim: WeakSimWorld, connection_id: ConnectionId) -> Self {
        Self { sim, connection_id }
    }
}

impl AsyncRead for SimTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown"))?;

        // Try to read from connection's receive buffer
        let mut temp_buf = vec![0u8; buf.remaining()];
        let bytes_read = sim
            .read_from_connection(self.connection_id, &mut temp_buf)
            .map_err(|e| io::Error::other(format!("read error: {}", e)))?;

        if bytes_read > 0 {
            buf.put_slice(&temp_buf[..bytes_read]);
        }

        // Note: In a real implementation, we would return Poll::Pending if no data
        // is available and register a waker. For Phase 2c, we simplify by
        // returning immediately with whatever data is available (0 bytes = EOF)
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SimTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown"))?;

        // Get write delay from network configuration
        let delay =
            sim.with_network_config_and_rng(|config, rng| config.latency.write_latency.sample(rng));

        // For Phase 2c: immediately write data to the connection's buffer
        // In a real implementation, this would be more complex with proper buffering
        sim.write_to_connection(self.connection_id, buf)
            .map_err(|e| io::Error::other(format!("write error: {}", e)))?;

        // Schedule the write completion event with configured delay
        sim.schedule_event(
            Event::DataDelivery {
                connection_id: self.connection_id.0,
                data: buf.to_vec(),
            },
            delay,
        );

        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// Simulated TCP listener
pub struct SimTcpListener {
    sim: WeakSimWorld,
    #[allow(dead_code)] // Will be used in future phases
    listener_id: ListenerId,
    local_addr: String,
}

impl SimTcpListener {
    /// Create a new simulated TCP listener
    pub(crate) fn new(sim: WeakSimWorld, listener_id: ListenerId, local_addr: String) -> Self {
        Self {
            sim,
            listener_id,
            local_addr,
        }
    }
}

#[async_trait(?Send)]
impl TcpListenerTrait for SimTcpListener {
    type TcpStream = SimTcpStream;

    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Get accept delay from network configuration
        let delay = sim
            .with_network_config_and_rng(|config, rng| config.latency.accept_latency.sample(rng));

        // Create a connection for the accepted client
        let connection_id = sim
            .create_connection("client-addr".to_string())
            .map_err(|_| io::Error::other("connection creation failed"))?;

        // Schedule accept completion event to advance simulation time
        sim.schedule_event(
            Event::ConnectionReady {
                connection_id: connection_id.0,
            },
            delay,
        );

        let stream = SimTcpStream::new(self.sim.clone(), connection_id);
        Ok((stream, "127.0.0.1:12345".to_string()))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.local_addr.clone())
    }
}
