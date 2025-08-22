use super::types::{ConnectionId, ListenerId};
use crate::network::traits::TcpListenerTrait;
use crate::{Event, WeakSimWorld};
use async_trait::async_trait;
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::instrument;

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
    #[instrument(skip(self, cx, buf))]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
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
            Poll::Ready(Ok(()))
        } else {
            // No data available - register for notification when data arrives
            sim.register_read_waker(self.connection_id, cx.waker().clone())
                .map_err(|e| io::Error::other(format!("waker registration error: {}", e)))?;
            Poll::Pending
        }
    }
}

impl AsyncWrite for SimTcpStream {
    #[instrument(skip(self, _cx, buf))]
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
        let delay = sim.with_network_config(|config| config.latency.write_latency.sample());

        // Schedule data delivery to paired connection with configured delay

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

/// Future representing an accept operation
pub struct AcceptFuture {
    sim: WeakSimWorld,
    local_addr: String,
    #[allow(dead_code)] // May be used in future phases for more sophisticated listener tracking
    listener_id: ListenerId,
}

impl Future for AcceptFuture {
    type Output = io::Result<(SimTcpStream, String)>;
    
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let sim = match self.sim.upgrade() {
            Ok(sim) => sim,
            Err(_) => return Poll::Ready(Err(io::Error::other("simulation shutdown"))),
        };
        
        match sim.get_pending_connection(&self.local_addr) {
            Ok(Some(connection_id)) => {
                // Get accept delay from network configuration
                let delay = sim.with_network_config(|config| config.latency.accept_latency.sample());
                
                // Schedule accept completion event to advance simulation time
                sim.schedule_event(
                    Event::ConnectionReady {
                        connection_id: connection_id.0,
                    },
                    delay,
                );
                
                let stream = SimTcpStream::new(self.sim.clone(), connection_id);
                Poll::Ready(Ok((stream, "127.0.0.1:12345".to_string())))
            },
            Ok(None) => {
                // No connection available yet - register waker for when connection becomes available
                if let Err(e) = sim.register_accept_waker(&self.local_addr, cx.waker().clone()) {
                    Poll::Ready(Err(io::Error::other(format!("failed to register accept waker: {}", e))))
                } else {
                    Poll::Pending
                }
            },
            Err(e) => Poll::Ready(Err(io::Error::other(format!("failed to get pending connection: {}", e)))),
        }
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

    #[instrument(skip(self))]
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        AcceptFuture {
            sim: self.sim.clone(),
            local_addr: self.local_addr.clone(),
            listener_id: self.listener_id,
        }.await
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.local_addr.clone())
    }
}
