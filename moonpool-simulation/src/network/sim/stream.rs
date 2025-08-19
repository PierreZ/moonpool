use super::types::{ConnectionId, ListenerId};
use crate::network::traits::TcpListenerTrait;
use crate::{Event, SimulationResult, WeakSimWorld};
use async_trait::async_trait;
use rand::Rng;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Simulated TCP stream
pub struct SimTcpStream {
    sim: WeakSimWorld,
    connection_id: ConnectionId,
    has_read: bool, // Track if we've already provided data
}

impl SimTcpStream {
    /// Create a new simulated TCP stream
    pub(crate) fn new(sim: WeakSimWorld, connection_id: ConnectionId) -> Self {
        Self {
            sim,
            connection_id,
            has_read: false,
        }
    }
}

impl AsyncRead for SimTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // Check if we've already provided data
        if this.has_read {
            // Return OK with no bytes added (EOF condition)
            // This is what AsyncRead does to signal end of stream
            return Poll::Ready(Ok(()));
        }

        let _sim = this
            .sim
            .upgrade()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown"))?;

        // For Phase 2b: simplified read simulation
        // Returns test data once to enable echo server functionality
        let test_data = b"Hello, echo server!";
        let bytes_to_copy = buf.remaining().min(test_data.len());

        // Only add data if there's space in the buffer
        if bytes_to_copy > 0 {
            buf.put_slice(&test_data[..bytes_to_copy]);
        }

        // Mark that we've provided data
        this.has_read = true;

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

        // Simulate write delay with random jitter
        let delay = sim.with_rng(|rng| {
            let base = Duration::from_micros(100);
            let jitter = Duration::from_micros(rng.gen_range(0..=500));
            base + jitter
        });

        // Schedule data delivery event (simplified for Phase 2b)
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
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "simulation shutdown"))?;

        // Simulate accept delay with random jitter
        let delay = sim.with_rng(|rng| {
            let base = Duration::from_millis(1);
            let jitter = Duration::from_millis(rng.gen_range(0..=5));
            base + jitter
        });

        // Use tokio sleep for the delay simulation
        tokio::time::sleep(delay).await;

        // For Phase 2b: simplified accept that immediately creates a connection
        let connection_id = sim
            .create_connection("client-addr".to_string())
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "connection creation failed"))?;

        let stream = SimTcpStream::new(self.sim.clone(), connection_id);
        Ok((stream, "127.0.0.1:12345".to_string()))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.local_addr.clone())
    }
}
