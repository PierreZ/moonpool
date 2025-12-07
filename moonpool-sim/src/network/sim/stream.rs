use super::types::{ConnectionId, ListenerId};
use crate::TcpListenerTrait;
use crate::sim::state::CloseReason;
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

/// Create an io::Error for simulation shutdown.
///
/// Used when the simulation world has been dropped but stream operations are still attempted.
fn sim_shutdown_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown")
}

/// Simulated TCP stream that implements async read/write operations.
///
/// `SimTcpStream` provides a realistic simulation of TCP socket behavior by implementing
/// the `AsyncRead` and `AsyncWrite` traits. It interfaces with the simulation event system
/// to provide ordered, reliable data delivery with configurable network delays.
///
/// ## Architecture Overview
///
/// Each `SimTcpStream` represents one endpoint of a TCP connection:
///
/// ```text
/// Application Layer          SimTcpStream Layer          Simulation Layer
/// ─────────────────          ──────────────────          ─────────────────
///                                                        
/// stream.write_all(data) ──► poll_write(data) ────────► buffer_send(data)
///                                                        └─► ProcessSendBuffer event
///                                                            └─► DataDelivery event
///                                                                └─► paired connection
///                                                        
/// stream.read(buf) ◄────── poll_read(buf) ◄──────────── receive_buffer
///                          │                           └─► waker registration
///                          └─► Poll::Pending/Ready     
/// ```
///
/// ## TCP Semantics Implemented
///
/// This implementation provides the core TCP guarantees required for realistic simulation:
///
/// ### 1. Reliable Delivery
/// - All written data will eventually be delivered to the paired connection
/// - No data loss (unless explicitly simulated via fault injection)
/// - Delivery confirmation through the event system
///
/// ### 2. Ordered Delivery (FIFO)
/// - Messages written first will arrive first at the destination
/// - Achieved through per-connection send buffering
/// - Critical for protocols that depend on message ordering
///
/// ### 3. Flow Control Simulation
/// - Read operations block (`Poll::Pending`) when no data is available
/// - Write operations complete immediately (buffering model)
/// - Backpressure handled at the application layer
///
/// ## Usage Examples
///
/// Provides async read/write operations for client and server connections.
///
/// ## Performance Characteristics
///
/// - **Write Latency**: O(1) - writes are buffered immediately
/// - **Read Latency**: O(network_delay) - depends on simulation configuration
/// - **Memory Usage**: O(buffered_data) - proportional to unread data
/// - **CPU Overhead**: Minimal - leverages efficient event system
///
/// ## Connection Lifecycle
///
/// 1. **Creation**: Stream created with reference to simulation and connection ID
/// 2. **Active Phase**: Read/write operations interact with simulation buffers
/// 3. **Data Transfer**: Asynchronous event processing handles network simulation
/// 4. **Termination**: Stream dropped when connection ends (automatic cleanup)
///
/// ## Thread Safety
///
/// `SimTcpStream` is designed for single-threaded simulation environments:
/// - No `Send` or `Sync` bounds (uses `#[async_trait(?Send)]`)
/// - Safe for use within single-threaded async runtimes
/// - Eliminates synchronization overhead for deterministic simulation
pub struct SimTcpStream {
    /// Weak reference to the simulation world.
    ///
    /// Uses `WeakSimWorld` to avoid circular references while allowing the stream
    /// to detect if the simulation has been dropped. Operations return errors
    /// gracefully if the simulation is no longer available.
    sim: WeakSimWorld,

    /// Unique identifier for this connection within the simulation.
    ///
    /// This ID corresponds to a `ConnectionState` entry in the simulation's
    /// connection table. Used to route read/write operations to the correct
    /// connection buffers and waker registrations.
    connection_id: ConnectionId,
}

impl SimTcpStream {
    /// Create a new simulated TCP stream
    pub(crate) fn new(sim: WeakSimWorld, connection_id: ConnectionId) -> Self {
        Self { sim, connection_id }
    }

    /// Get the connection ID (for test introspection and chaos injection)
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }
}

impl Drop for SimTcpStream {
    fn drop(&mut self) {
        // Close the connection in the simulation when the stream is dropped
        // This matches real TCP behavior where dropping a socket always closes it
        if let Ok(sim) = self.sim.upgrade() {
            tracing::debug!(
                "SimTcpStream dropping, closing connection {}",
                self.connection_id.0
            );
            sim.close_connection(self.connection_id);
        }
    }
}

impl AsyncRead for SimTcpStream {
    #[instrument(skip(self, cx, buf))]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        tracing::trace!(
            "SimTcpStream::poll_read called on connection_id={}",
            self.connection_id.0
        );
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Random close chaos injection (FDB rollRandomClose pattern)
        // Check at start of every read operation - sim2.actor.cpp:408
        // Returns Some(true) for explicit error, Some(false) for silent (connection marked closed)
        if let Some(true) = sim.roll_random_close(self.connection_id) {
            // 30% explicit exception - throw connection_failed immediately
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "Random connection failure (explicit)",
            )));
            // 70% silent case: connection already marked as closed, will return EOF below
        }

        // Check if receive side is closed (asymmetric closure)
        if sim.is_recv_closed(self.connection_id) {
            tracing::debug!(
                "SimTcpStream::poll_read connection_id={} recv side closed, returning EOF",
                self.connection_id.0
            );
            return Poll::Ready(Ok(())); // EOF
        }

        // Check for half-open connection (peer crashed)
        if sim.is_half_open(self.connection_id) && sim.should_half_open_error(self.connection_id) {
            // Error time reached - return ECONNRESET
            tracing::debug!(
                "SimTcpStream::poll_read connection_id={} half-open error time reached, returning ECONNRESET",
                self.connection_id.0
            );
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "Connection reset (half-open timeout)",
            )));
        }
        // Half-open but not yet error time - will block (Pending) below waiting for data
        // that will never come, which is the correct half-open behavior

        // Check for read clogging (symmetric with write clogging)
        if sim.is_read_clogged(self.connection_id) {
            // Already clogged, register waker and return Pending
            sim.register_read_clog_waker(self.connection_id, cx.waker().clone());
            return Poll::Pending;
        }

        // Check if this read should be clogged
        if sim.should_clog_read(self.connection_id) {
            sim.clog_read(self.connection_id);
            sim.register_read_clog_waker(self.connection_id, cx.waker().clone());
            return Poll::Pending;
        }

        // Try to read from connection's receive buffer first
        // We should be able to read buffered data even if connection is currently cut
        let mut temp_buf = vec![0u8; buf.remaining()];
        let bytes_read = sim
            .read_from_connection(self.connection_id, &mut temp_buf)
            .map_err(|e| io::Error::other(format!("read error: {}", e)))?;

        tracing::trace!(
            "SimTcpStream::poll_read connection_id={} read {} bytes",
            self.connection_id.0,
            bytes_read
        );

        if bytes_read > 0 {
            let data_preview = String::from_utf8_lossy(&temp_buf[..std::cmp::min(bytes_read, 20)]);
            tracing::trace!(
                "SimTcpStream::poll_read connection_id={} returning data: '{}'",
                self.connection_id.0,
                data_preview
            );
            buf.put_slice(&temp_buf[..bytes_read]);
            Poll::Ready(Ok(()))
        } else {
            // No data available - check if connection is closed or cut
            if sim.is_connection_closed(self.connection_id) {
                // Check how the connection was closed
                match sim.get_close_reason(self.connection_id) {
                    CloseReason::Aborted => {
                        tracing::info!(
                            "SimTcpStream::poll_read connection_id={} was aborted (RST), returning ECONNRESET",
                            self.connection_id.0
                        );
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::ConnectionReset,
                            "Connection was aborted (RST)",
                        )));
                    }
                    _ => {
                        tracing::info!(
                            "SimTcpStream::poll_read connection_id={} is closed gracefully (FIN), returning EOF (0 bytes)",
                            self.connection_id.0
                        );
                        // Connection closed gracefully (FIN) - return EOF (0 bytes read)
                        return Poll::Ready(Ok(()));
                    }
                }
            }

            if sim.is_connection_cut(self.connection_id) {
                // Connection is temporarily cut - register waker and wait for restoration
                tracing::debug!(
                    "SimTcpStream::poll_read connection_id={} is cut, registering cut waker",
                    self.connection_id.0
                );
                sim.register_cut_waker(self.connection_id, cx.waker().clone());
                return Poll::Pending;
            }

            // Register for notification when data arrives
            tracing::trace!(
                "SimTcpStream::poll_read connection_id={} no data, registering waker",
                self.connection_id.0
            );
            sim.register_read_waker(self.connection_id, cx.waker().clone())
                .map_err(|e| io::Error::other(format!("waker registration error: {}", e)))?;

            // Double-check for data after registering waker to handle race conditions
            // This prevents deadlocks where DataDelivery arrives between initial check and waker registration
            let mut temp_buf_recheck = vec![0u8; buf.remaining()];
            let bytes_read_recheck = sim
                .read_from_connection(self.connection_id, &mut temp_buf_recheck)
                .map_err(|e| io::Error::other(format!("recheck read error: {}", e)))?;

            if bytes_read_recheck > 0 {
                let data_preview = String::from_utf8_lossy(
                    &temp_buf_recheck[..std::cmp::min(bytes_read_recheck, 20)],
                );
                tracing::trace!(
                    "SimTcpStream::poll_read connection_id={} found data on recheck: '{}' (race condition avoided)",
                    self.connection_id.0,
                    data_preview
                );
                buf.put_slice(&temp_buf_recheck[..bytes_read_recheck]);
                Poll::Ready(Ok(()))
            } else {
                // Final check - if connection is closed or cut and no data available
                if sim.is_connection_closed(self.connection_id) {
                    match sim.get_close_reason(self.connection_id) {
                        CloseReason::Aborted => {
                            tracing::info!(
                                "SimTcpStream::poll_read connection_id={} was aborted on recheck (RST), returning ECONNRESET",
                                self.connection_id.0
                            );
                            Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::ConnectionReset,
                                "Connection was aborted (RST)",
                            )))
                        }
                        _ => {
                            tracing::info!(
                                "SimTcpStream::poll_read connection_id={} is closed on recheck (FIN), returning EOF (0 bytes)",
                                self.connection_id.0
                            );
                            // Connection closed gracefully - return EOF (0 bytes read)
                            Poll::Ready(Ok(()))
                        }
                    }
                } else if sim.is_connection_cut(self.connection_id) {
                    // Connection is temporarily cut - already registered waker above, just wait
                    tracing::debug!(
                        "SimTcpStream::poll_read connection_id={} is cut on recheck, waiting",
                        self.connection_id.0
                    );
                    Poll::Pending
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl AsyncWrite for SimTcpStream {
    #[instrument(skip(self, cx, buf))]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Random close chaos injection (FDB rollRandomClose pattern)
        // Check at start of every write operation - sim2.actor.cpp:423
        // Returns Some(true) for explicit error, Some(false) for silent (connection marked closed)
        if let Some(true) = sim.roll_random_close(self.connection_id) {
            // 30% explicit exception - throw connection_failed immediately
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "Random connection failure (explicit)",
            )));
            // 70% silent case: connection already marked as closed, will fail below
        }

        // Check if send side is closed (asymmetric closure)
        if sim.is_send_closed(self.connection_id) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Connection send side closed",
            )));
        }

        // Check if connection is closed or cut
        if sim.is_connection_closed(self.connection_id) {
            // Check how the connection was closed
            return match sim.get_close_reason(self.connection_id) {
                CloseReason::Aborted => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "Connection was aborted (RST)",
                ))),
                _ => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Connection was closed (FIN)",
                ))),
            };
        }

        if sim.is_connection_cut(self.connection_id) {
            // Connection is temporarily cut - register waker and wait for restoration
            tracing::debug!(
                "SimTcpStream::poll_write connection_id={} is cut, registering cut waker",
                self.connection_id.0
            );
            sim.register_cut_waker(self.connection_id, cx.waker().clone());
            tracing::debug!(
                "SimTcpStream::poll_write connection_id={} registered waker for cut connection",
                self.connection_id.0
            );
            return Poll::Pending;
        }

        // Check for half-open connection (peer crashed)
        if sim.is_half_open(self.connection_id) && sim.should_half_open_error(self.connection_id) {
            // Error time reached - return ECONNRESET
            tracing::debug!(
                "SimTcpStream::poll_write connection_id={} half-open error time reached, returning ECONNRESET",
                self.connection_id.0
            );
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "Connection reset (half-open timeout)",
            )));
        }
        // Half-open but not yet error time - writes succeed but data goes nowhere
        // (paired_connection is already None, so buffer_send will silently succeed)

        // Check for send buffer space (backpressure)
        let available_buffer = sim.available_send_buffer(self.connection_id);
        if available_buffer < buf.len() {
            // Not enough buffer space, register waker and return Pending
            tracing::debug!(
                "SimTcpStream::poll_write connection_id={} buffer full (available={}, needed={}), waiting",
                self.connection_id.0,
                available_buffer,
                buf.len()
            );
            sim.register_send_buffer_waker(self.connection_id, cx.waker().clone());
            return Poll::Pending;
        }

        // Phase 7: Check for write clogging
        if sim.is_write_clogged(self.connection_id) {
            // Already clogged, register waker and return Pending
            sim.register_clog_waker(self.connection_id, cx.waker().clone());
            return Poll::Pending;
        }

        // Check if this write should be clogged
        if sim.should_clog_write(self.connection_id) {
            sim.clog_write(self.connection_id);
            sim.register_clog_waker(self.connection_id, cx.waker().clone());
            return Poll::Pending;
        }

        // Use buffered send to maintain TCP ordering
        let data_preview = String::from_utf8_lossy(&buf[..std::cmp::min(buf.len(), 20)]);
        tracing::trace!(
            "SimTcpStream::poll_write buffering {} bytes: '{}' for ordered delivery",
            buf.len(),
            data_preview
        );

        // Buffer the data for ordered processing instead of direct event scheduling
        sim.buffer_send(self.connection_id, buf.to_vec())
            .map_err(|e| io::Error::other(format!("buffer send error: {}", e)))?;

        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Close the connection in the simulation when shutdown is called
        tracing::debug!(
            "SimTcpStream::poll_shutdown closing connection {}",
            self.connection_id.0
        );
        sim.close_connection(self.connection_id);

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
            Err(_) => return Poll::Ready(Err(sim_shutdown_error())),
        };

        match sim.get_pending_connection(&self.local_addr) {
            Ok(Some(connection_id)) => {
                // Get accept delay from network configuration
                let delay = sim.with_network_config(|config| {
                    crate::network::sample_duration(&config.accept_latency)
                });

                // Schedule accept completion event to advance simulation time
                sim.schedule_event(
                    Event::Connection {
                        id: connection_id.0,
                        state: crate::ConnectionStateChange::ConnectionReady,
                    },
                    delay,
                );

                // FDB Pattern (sim2.actor.cpp:1149-1175):
                // Return the synthesized ephemeral peer address, not the client's real address.
                // This simulates real TCP where servers see client ephemeral ports.
                let peer_addr = sim
                    .get_connection_peer_address(connection_id)
                    .unwrap_or_else(|| "unknown:0".to_string());

                let stream = SimTcpStream::new(self.sim.clone(), connection_id);
                Poll::Ready(Ok((stream, peer_addr)))
            }
            Ok(None) => {
                // No connection available yet - register waker for when connection becomes available
                if let Err(e) = sim.register_accept_waker(&self.local_addr, cx.waker().clone()) {
                    Poll::Ready(Err(io::Error::other(format!(
                        "failed to register accept waker: {}",
                        e
                    ))))
                } else {
                    Poll::Pending
                }
            }
            Err(e) => Poll::Ready(Err(io::Error::other(format!(
                "failed to get pending connection: {}",
                e
            )))),
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
        }
        .await
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.local_addr.clone())
    }
}
