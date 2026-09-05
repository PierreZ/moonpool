use super::{AcceptWaiterId, CloseReason, NetworkDelay, types::ConnectionId};
use crate::{TcpListenerTrait, WeakSimWorld};
use futures::io::{AsyncRead, AsyncWrite};
use std::{
    future::Future,
    io::{self, IoSlice},
    pin::Pin,
    task::{Context, Poll},
};
use tracing::instrument;

/// Create an `io::Error` for simulation shutdown.
///
/// Used when the simulation world has been dropped but stream operations are still attempted.
fn sim_shutdown_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown")
}

/// Create an `io::Error` for random connection failure (chaos injection).
fn random_connection_failure_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionReset,
        "Random connection failure (explicit)",
    )
}

/// Create an `io::Error` for aborted connection (RST).
fn connection_aborted_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionReset,
        "Connection was aborted (RST)",
    )
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
///                                                            └─► in-flight queue
///                                                                └─► Delivery event
///                                                                    └─► peer receive_buffer
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
/// ### 3. Flow Control
/// - Read operations block (`Poll::Pending`) when no data is available
/// - Each direction has an end-to-end byte window
///   ([`NetworkConfiguration::tcp_send_window_bytes`](crate::NetworkConfiguration::tcp_send_window_bytes)):
///   a write accepts at most what the window has left (a short write when
///   some room remains) and parks when it has none
/// - The window is taken when the write is accepted and returned only when
///   the *peer's application reads* the bytes, so a slow reader backs the
///   writer up through the send queue, the flight and the peer's receive
///   buffer alike; a direction that stops delivering (partition, black hole)
///   fills its window and blocks instead of accepting data forever
/// - A parked writer is woken by the peer's read, or by the connection
///   failing (an abort wakes it to the error; it is never left waiting on a
///   destroyed connection)
///
/// ## Usage Examples
///
/// Provides async read/write operations for client and server connections.
///
/// ## Performance Characteristics
///
/// - **Write Latency**: O(1) while the window has room
/// - **Read Latency**: `O(network_delay)` - depends on simulation configuration
/// - **Memory Usage**: `O(buffered_data)` - proportional to unread data
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
/// `SimTcpStream` is `Send + Sync + Unpin + 'static` via its `Arc<RwLock<…>>`
/// backed `WeakSimWorld` handle. The simulation runtime itself runs on a
/// single OS thread (`new_current_thread().build()`), but the stream type
/// satisfies Send-bounded traits so it composes naturally with
/// `tokio::spawn`, hyper/reqwest connectors, and customer code that uses
/// `Arc<RwLock<…>>` / `DashMap` / `Arc<AtomicBool>`.
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
    #[must_use]
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    /// Returns `true`: `SimTcpStream` implements an efficient vectored write that
    /// records each `IoSlice` as its own ordered delivery event, so the chaos pack
    /// can act on individual segments.
    #[must_use]
    pub fn is_write_vectored(&self) -> bool {
        true
    }

    /// Run the closure checks that precede backpressure for a write.
    ///
    /// Returns `Some(poll)` to short-circuit, `None` to proceed.
    fn write_guard_pre_backpressure(
        &self,
        sim: &crate::sim::SimWorld,
    ) -> Option<Poll<Result<usize, io::Error>>> {
        // Check if send side is closed (asymmetric closure)
        if sim.is_send_closed(self.connection_id) {
            return Some(Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Connection send side closed",
            ))));
        }

        // Check if connection is closed.
        if sim.is_connection_closed(self.connection_id) {
            // Check how the connection was closed
            return Some(match sim.close_reason(self.connection_id) {
                CloseReason::Aborted => Poll::Ready(Err(connection_aborted_error())),
                _ => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Connection was closed (FIN)",
                ))),
            });
        }

        None
    }

    /// Run the write-clog checks after the backpressure decision.
    ///
    /// Returns `Some(poll)` to short-circuit, `None` to proceed.
    fn write_guard_clog(
        &self,
        sim: &crate::sim::SimWorld,
        cx: &mut Context<'_>,
    ) -> Option<Poll<Result<usize, io::Error>>> {
        // Phase 7: Check for write clogging
        if sim.is_write_clogged(self.connection_id) {
            // Already clogged, register waker and return Pending
            if !sim.register_clog_waker(self.connection_id, cx.waker()) {
                return Some(Poll::Ready(Err(sim_shutdown_error())));
            }
            return Some(Poll::Pending);
        }

        // Check if this write should be clogged
        if sim.should_clog_write(self.connection_id) {
            sim.clog_write(self.connection_id);
            if !sim.register_clog_waker(self.connection_id, cx.waker()) {
                return Some(Poll::Ready(Err(sim_shutdown_error())));
            }
            return Some(Poll::Pending);
        }

        None
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
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        tracing::trace!(
            "SimTcpStream::poll_read called on connection_id={}",
            self.connection_id.0
        );
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // `is_recv_closed` includes fully closed connections. Preserve the
        // stronger RST signal before treating an asymmetrically closed receive
        // side as EOF.
        if sim.is_connection_closed(self.connection_id)
            && sim.close_reason(self.connection_id) == CloseReason::Aborted
        {
            return Poll::Ready(Err(connection_aborted_error()));
        }

        // Check if receive side is closed (asymmetric closure)
        if sim.is_recv_closed(self.connection_id) {
            tracing::debug!(
                "SimTcpStream::poll_read connection_id={} recv side closed, returning EOF",
                self.connection_id.0
            );
            return Poll::Ready(Ok(0)); // EOF
        }

        // Check for read clogging (symmetric with write clogging)
        if sim.is_read_clogged(self.connection_id) {
            // Already clogged, register waker and return Pending
            if !sim.register_read_clog_waker(self.connection_id, cx.waker()) {
                return Poll::Ready(Err(sim_shutdown_error()));
            }
            return Poll::Pending;
        }

        // A parked read is not an I/O operation yet. In particular, do not
        // consume simulation entropy merely because an executor polls h2 again
        // while the socket still has no data. Poll counts are not semantic and
        // may vary independently of the simulated execution.
        if !sim.has_readable_data(self.connection_id) {
            return Self::poll_read_no_data(&sim, self.connection_id, cx, buf);
        }

        // Random close chaos injection (FDB rollRandomClose pattern). Sample
        // only once the read can make progress, so a no-data `Poll::Pending`
        // cannot shift the global simulation RNG stream.
        if let Some(true) = sim.roll_random_close(self.connection_id) {
            return Poll::Ready(Err(random_connection_failure_error()));
        }
        // The black hole rolls beside the close, under the same progress
        // rule, on its own coin. It changes nothing about this operation.
        sim.roll_black_hole(self.connection_id);
        if sim.is_connection_closed(self.connection_id)
            && sim.close_reason(self.connection_id) == CloseReason::Aborted
        {
            return Poll::Ready(Err(connection_aborted_error()));
        }
        if sim.is_recv_closed(self.connection_id) {
            return Poll::Ready(Ok(0));
        }

        // Check if this read should be clogged
        if sim.should_clog_read(self.connection_id) {
            sim.clog_read(self.connection_id);
            if !sim.register_read_clog_waker(self.connection_id, cx.waker()) {
                return Poll::Ready(Err(sim_shutdown_error()));
            }
            return Poll::Pending;
        }

        // Try to read from connection's receive buffer first
        // We should be able to read buffered data even if connection is currently cut
        let bytes_read = sim
            .read_from_connection(self.connection_id, buf)
            .map_err(|e| io::Error::other(format!("read error: {e}")))?;

        tracing::trace!(
            "SimTcpStream::poll_read connection_id={} read {} bytes",
            self.connection_id.0,
            bytes_read
        );

        debug_assert!(bytes_read > 0, "readable connection returned no bytes");
        let data_preview = String::from_utf8_lossy(&buf[..bytes_read.min(20)]);
        tracing::trace!(
            "SimTcpStream::poll_read connection_id={} returning data: '{}'",
            self.connection_id.0,
            data_preview
        );
        Poll::Ready(Ok(bytes_read))
    }
}

impl SimTcpStream {
    /// Handle the `poll_read` branch where no buffered data is currently
    /// available. Checks for graceful FIN or abort, then registers a
    /// read waker and rechecks the buffer to avoid races.
    fn poll_read_no_data(
        sim: &crate::sim::SimWorld,
        connection_id: crate::network::sim::ConnectionId,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // No data available - check if connection has received FIN or is closed.
        if let Some(result) = Self::read_terminal_result(sim, connection_id) {
            return Poll::Ready(result);
        }
        // Register for notification when data arrives, then re-check for race.
        tracing::trace!(
            "SimTcpStream::poll_read connection_id={} no data, registering waker",
            connection_id.0
        );
        if !sim.register_read_waker(connection_id, cx.waker()) {
            return Poll::Ready(Err(sim_shutdown_error()));
        }

        let bytes_read = sim
            .read_from_connection(connection_id, buf)
            .map_err(|e| io::Error::other(format!("recheck read error: {e}")))?;

        if bytes_read > 0 {
            return Poll::Ready(Ok(bytes_read));
        }

        // Final check after waker registration.
        if let Some(result) = Self::read_terminal_result(sim, connection_id) {
            return Poll::Ready(result);
        }
        Poll::Pending
    }

    /// Return the result of a terminal receive state, if the connection reached one.
    fn read_terminal_result(
        sim: &crate::sim::SimWorld,
        connection_id: ConnectionId,
    ) -> Option<io::Result<usize>> {
        if sim.is_remote_fin_received(connection_id) {
            tracing::info!(
                "SimTcpStream::poll_read connection_id={} remote FIN received, returning EOF",
                connection_id.0
            );
            return Some(Ok(0));
        }
        if !sim.is_connection_closed(connection_id) {
            return None;
        }
        if sim.close_reason(connection_id) == CloseReason::Aborted {
            tracing::info!(
                "SimTcpStream::poll_read connection_id={} was aborted (RST)",
                connection_id.0
            );
            return Some(Err(connection_aborted_error()));
        }
        tracing::info!(
            "SimTcpStream::poll_read connection_id={} closed gracefully (FIN)",
            connection_id.0
        );
        Some(Ok(0))
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

        if let Some(poll) = self.write_guard_pre_backpressure(&sim) {
            return poll;
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // Match write(2): accept a short write when some window remains and
        // park only when the window has no room at all. The window is
        // end-to-end (queued + in flight + unread at the peer), so this is
        // where a slow reader shows up as backpressure.
        let available = sim.available_send_bytes(self.connection_id);
        if available == 0 {
            tracing::debug!(
                "SimTcpStream::poll_write connection_id={} window full (needed={}), waiting",
                self.connection_id.0,
                buf.len()
            );
            if !sim.register_send_buffer_waker(self.connection_id, cx.waker()) {
                return Poll::Ready(Err(sim_shutdown_error()));
            }
            return Poll::Pending;
        }

        // A full window makes this a no-progress poll, not a fresh I/O
        // operation. Roll random-close chaos only after capacity is available
        // so backpressure repolls cannot perturb deterministic replay.
        if let Some(true) = sim.roll_random_close(self.connection_id) {
            return Poll::Ready(Err(random_connection_failure_error()));
        }
        // The black hole rolls beside the close, under the same progress
        // rule, on its own coin. It changes nothing about this operation.
        sim.roll_black_hole(self.connection_id);
        if let Some(poll) = self.write_guard_pre_backpressure(&sim) {
            return poll;
        }

        if let Some(poll) = self.write_guard_clog(&sim, cx) {
            return poll;
        }

        // Use buffered send to maintain TCP ordering.
        let accepted = buf.len().min(available);
        let accepted_data = &buf[..accepted];
        let data_preview = String::from_utf8_lossy(&accepted_data[..accepted.min(20)]);
        tracing::trace!(
            "SimTcpStream::poll_write buffering {} bytes: '{}' for ordered delivery",
            accepted,
            data_preview
        );

        // Buffer the data for ordered processing instead of direct event scheduling
        sim.buffer_send(self.connection_id, accepted_data.to_vec())
            .map_err(|e| io::Error::other(format!("buffer send error: {e}")))?;

        Poll::Ready(Ok(accepted))
    }

    #[instrument(skip(self, cx, bufs))]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        if let Some(poll) = self.write_guard_pre_backpressure(&sim) {
            return poll;
        }

        let total: usize = bufs.iter().map(|slice| slice.len()).sum();
        if total == 0 {
            return Poll::Ready(Ok(0));
        }

        // writev(2) partial-accept semantics: if there's SOME room, accept what
        // fits and report a short count; only block when there is NO room at all.
        let available = sim.available_send_bytes(self.connection_id);
        if available == 0 {
            if !sim.register_send_buffer_waker(self.connection_id, cx.waker()) {
                return Poll::Ready(Err(sim_shutdown_error()));
            }
            return Poll::Pending;
        }

        if let Some(true) = sim.roll_random_close(self.connection_id) {
            return Poll::Ready(Err(random_connection_failure_error()));
        }
        // The black hole rolls beside the close, under the same progress
        // rule, on its own coin. It changes nothing about this operation.
        sim.roll_black_hole(self.connection_id);
        if let Some(poll) = self.write_guard_pre_backpressure(&sim) {
            return poll;
        }

        if let Some(poll) = self.write_guard_clog(&sim, cx) {
            return poll;
        }

        let accepted = total.min(available);

        // Buffer each IoSlice as its own ordered delivery event, truncating the
        // boundary slice when `accepted < total`. Skip empty slices so they do not
        // create empty delivery events.
        let mut remaining = accepted;
        for slice in bufs {
            if remaining == 0 {
                break;
            }
            if slice.is_empty() {
                continue;
            }
            let take = remaining.min(slice.len());
            sim.buffer_send(self.connection_id, slice[..take].to_vec())
                .map_err(|e| io::Error::other(format!("buffer send error: {e}")))?;
            remaining -= take;
        }

        Poll::Ready(Ok(accepted))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let sim = self.sim.upgrade().map_err(|_| sim_shutdown_error())?;

        // Close the connection in the simulation when close is called
        tracing::debug!(
            "SimTcpStream::poll_close closing connection {}",
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
    reserved: Option<ConnectionId>,
    delay: Option<NetworkDelay>,
    waiter_id: Option<AcceptWaiterId>,
}

impl Future for AcceptFuture {
    type Output = io::Result<(SimTcpStream, String)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Ok(sim) = self.sim.upgrade() else {
            return Poll::Ready(Err(sim_shutdown_error()));
        };
        if self.waiter_id.is_none() {
            let id = match sim.allocate_accept_waiter() {
                Ok(id) => id,
                Err(error) => return Poll::Ready(Err(io::Error::other(error))),
            };
            self.waiter_id = Some(id);
        }
        if self.reserved.is_none() {
            let Some(waiter_id) = self.waiter_id else {
                return Poll::Ready(Err(io::Error::other("accept waiter missing")));
            };
            let connection_id =
                match sim.poll_accept(&self.local_addr, waiter_id, cx.waker().clone()) {
                    Ok(Some(connection_id)) => connection_id,
                    Ok(None) => return Poll::Pending,
                    Err(error) => return Poll::Ready(Err(io::Error::other(error))),
                };
            let delay = sim.with_network_config(|config| {
                crate::network::sample_latency(&config.accept_latency)
            });
            let operation = match sim.network_delay(delay) {
                Ok(operation) => operation,
                Err(error) => {
                    sim.return_pending_connection(&self.local_addr, connection_id);
                    return Poll::Ready(Err(io::Error::other(error)));
                }
            };
            self.reserved = Some(connection_id);
            self.delay = Some(operation);
        }

        let Some(delay) = self.delay.as_mut() else {
            return Poll::Ready(Err(io::Error::other("accept delay state missing")));
        };
        match Pin::new(delay).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(io::Error::other(error))),
            Poll::Ready(Ok(())) => {}
        }
        self.delay.take();
        let Some(connection_id) = self.reserved.take() else {
            return Poll::Ready(Err(io::Error::other("accept reservation missing")));
        };

        // FDB Pattern (sim2.actor.cpp:1149-1175):
        // Return the synthesized ephemeral peer address, not the client's real address.
        // This simulates real TCP where servers see client ephemeral ports.
        let peer_addr = sim
            .connection_peer_address(connection_id)
            .unwrap_or_else(|| "unknown:0".to_string());

        let stream = SimTcpStream::new(self.sim.clone(), connection_id);
        Poll::Ready(Ok((stream, peer_addr)))
    }
}

impl Drop for AcceptFuture {
    fn drop(&mut self) {
        self.delay.take();
        if let Ok(sim) = self.sim.upgrade() {
            if let Some(connection_id) = self.reserved.take()
                && !sim.is_connection_closed(connection_id)
            {
                sim.return_pending_connection(&self.local_addr, connection_id);
            }
            if let Some(waiter_id) = self.waiter_id.take() {
                sim.cancel_accept(&self.local_addr, waiter_id);
            }
        }
    }
}

/// Simulated TCP listener
pub struct SimTcpListener {
    sim: WeakSimWorld,
    local_addr: String,
}

impl SimTcpListener {
    /// Create a new simulated TCP listener
    pub(crate) fn new(sim: WeakSimWorld, local_addr: String) -> Self {
        Self { sim, local_addr }
    }
}

impl TcpListenerTrait for SimTcpListener {
    type TcpStream = SimTcpStream;

    #[instrument(skip(self))]
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        AcceptFuture {
            sim: self.sim.clone(),
            local_addr: self.local_addr.clone(),
            reserved: None,
            delay: None,
            waiter_id: None,
        }
        .await
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.local_addr.clone())
    }
}
