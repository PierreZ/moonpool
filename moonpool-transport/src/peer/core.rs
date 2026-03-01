//! Core peer implementation with automatic reconnection, message queuing,
//! and connection health monitoring.
//!
//! Provides wire format with UID-based endpoint addressing.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use super::config::{MonitorConfig, PeerConfig};
use super::error::{PeerError, PeerResult};
use super::metrics::PeerMetrics;
use crate::{
    HEADER_SIZE, NetworkProvider, Providers, TaskProvider, TimeProvider, UID, WireError,
    serialize_packet, try_deserialize_packet,
};
use moonpool_sim::{assert_sometimes, assert_sometimes_each};

// =============================================================================
// Ping/Pong Protocol Constants
// =============================================================================

/// Wire token for ping requests (uses existing `WellKnownToken::Ping`).
///
/// Intercepted by `connection_task`, never delivered to application.
///
/// FDB ref: `WLTOKEN_PING_PACKET` in `FlowTransport.h`
const PING_TOKEN: UID = UID::well_known(1);

/// Wire token for pong replies.
///
/// Uses a non-well-known UID (`first != u64::MAX`) to avoid conflicts
/// with the endpoint dispatch system. Intercepted by `connection_task`,
/// never delivered to application.
const PONG_TOKEN: UID = UID::new(u64::MAX - 1, 0);

// =============================================================================
// Ping Tracker State Machine
// =============================================================================

/// Action to take after a ping timer event.
enum PingAction {
    /// Send a ping packet to the remote peer.
    SendPing,
    /// Tolerate the timeout (bytes were received, connection alive but slow).
    Tolerate,
    /// Tear down the connection (unresponsive).
    TearDown,
}

/// Tracks ping/pong state within `connection_task`.
///
/// Implements the state machine:
/// ```text
/// Idle ──(interval timer)──► SendPing ──► AwaitingPong
/// AwaitingPong ──(pong received)──────► Idle (record RTT)
/// AwaitingPong ──(timeout + bytes changed)──► re-ping (tolerate)
/// AwaitingPong ──(timeout + no bytes)────► TearDown
/// AwaitingPong ──(>max_tolerated)────► TearDown
/// ```
///
/// FDB ref: `connectionMonitor` (`FlowTransport.actor.cpp:616-699`)
struct PingTracker {
    /// Monitoring configuration.
    config: MonitorConfig,

    /// Simulation time when the outstanding ping was sent.
    /// `Some` means we are in `AwaitingPong` phase; `None` means `Idle`.
    ping_sent_at: Option<Duration>,

    /// When the last ping cycle started (for computing next ping interval).
    last_ping_cycle: Option<Duration>,

    /// Bytes received at the time the ping was sent. Used to detect
    /// if the connection is still active even when pong is delayed.
    bytes_at_ping: u64,

    /// Consecutive timeout count for the current ping cycle.
    ///
    /// FDB: `timeoutCount` in `connectionMonitor`
    timeout_count: u32,
}

impl PingTracker {
    fn new(config: MonitorConfig) -> Self {
        Self {
            config,
            ping_sent_at: None,
            last_ping_cycle: None,
            bytes_at_ping: 0,
            timeout_count: 0,
        }
    }

    /// Compute how long until the next ping action should occur.
    fn time_until_next_action(&self, now: Duration) -> Duration {
        match self.ping_sent_at {
            Some(sent_at) => {
                // AwaitingPong — timer is the ping timeout
                let elapsed = now.saturating_sub(sent_at);
                self.config.ping_timeout.saturating_sub(elapsed)
            }
            None => {
                // Idle — timer is the ping interval
                let last = self.last_ping_cycle.unwrap_or(now);
                let elapsed = now.saturating_sub(last);
                self.config.ping_interval.saturating_sub(elapsed)
            }
        }
    }

    /// Called when a pong is received. Returns the measured RTT.
    fn on_pong_received(&mut self, now: Duration) -> Option<Duration> {
        if let Some(sent_at) = self.ping_sent_at.take() {
            self.timeout_count = 0;
            self.last_ping_cycle = Some(now);
            Some(now.saturating_sub(sent_at))
        } else {
            // Spurious pong (no outstanding ping), ignore
            None
        }
    }

    /// Called when the ping timer fires. Returns the action to take.
    fn on_timer_fired(&mut self, now: Duration, current_bytes_received: u64) -> PingAction {
        if self.ping_sent_at.is_some() {
            // We were awaiting pong and timed out
            self.timeout_count += 1;

            if current_bytes_received > self.bytes_at_ping {
                // Connection is still active (receiving data), tolerate
                self.bytes_at_ping = current_bytes_received;
                if self.timeout_count > self.config.max_tolerated_timeouts {
                    PingAction::TearDown
                } else {
                    PingAction::Tolerate
                }
            } else {
                // No bytes received since ping — connection is dead
                PingAction::TearDown
            }
        } else {
            // Idle timer fired — time to send a ping
            self.ping_sent_at = Some(now);
            self.bytes_at_ping = current_bytes_received;
            self.timeout_count = 0;
            self.last_ping_cycle = Some(now);
            PingAction::SendPing
        }
    }

    /// Reset tracker state (called on connection loss or reconnection).
    fn reset(&mut self) {
        self.ping_sent_at = None;
        self.last_ping_cycle = None;
        self.bytes_at_ping = 0;
        self.timeout_count = 0;
    }
}

// =============================================================================
// Peer Public API
// =============================================================================

/// Type alias for the peer receiver channel.
/// Used when taking ownership via `take_receiver()`.
pub type PeerReceiver = mpsc::UnboundedReceiver<(UID, Vec<u8>)>;

/// State for managing reconnections with exponential backoff.
#[derive(Debug, Clone)]
struct ReconnectState {
    /// Current backoff delay
    current_delay: Duration,

    /// Number of consecutive failures
    failure_count: u32,

    /// Time of last connection attempt (simulation or wall time as Duration)
    last_attempt: Option<Duration>,

    /// Whether we're currently in the process of reconnecting
    reconnecting: bool,
}

impl ReconnectState {
    fn new(initial_delay: Duration) -> Self {
        Self {
            current_delay: initial_delay,
            failure_count: 0,
            last_attempt: None,
            reconnecting: false,
        }
    }

    fn reset(&mut self, initial_delay: Duration) {
        self.current_delay = initial_delay;
        self.failure_count = 0;
        self.last_attempt = None;
        self.reconnecting = false;
    }
}

/// A resilient peer that manages connections to a remote address.
///
/// Provides automatic reconnection and message queuing while abstracting
/// over provider implementations via the [`Providers`] trait bundle.
///
/// Uses wire format: `[length:4][checksum:4][token:16][payload]`
///
/// Follows FoundationDB's architecture: synchronous API with background actors.
pub struct Peer<P: Providers> {
    /// Shared state accessible to background actors
    shared_state: Rc<RefCell<PeerSharedState<P>>>,

    /// Trigger to wake writer actor when data is queued
    data_to_send: Rc<Notify>,

    /// Background actor handles
    writer_handle: Option<JoinHandle<()>>,

    /// Receive channel for incoming packets (token + payload).
    /// Can be taken via `take_receiver()` for external ownership.
    receive_rx: Option<mpsc::UnboundedReceiver<(UID, Vec<u8>)>>,

    /// Shutdown signaling
    shutdown_tx: mpsc::UnboundedSender<()>,

    /// Configuration (owned by Peer)
    config: PeerConfig,

    /// Providers bundle for spawning background actors
    #[allow(dead_code)]
    providers: P,
}

/// Shared state for background actors - each actor accesses different fields
struct PeerSharedState<P: Providers> {
    /// Network provider for creating connections
    network: P::Network,

    /// Time provider for delays and timing
    time: P::Time,

    /// Destination address
    destination: String,

    /// Connection status (actual stream owned by writer actor)
    connection: Option<()>, // Just tracks if connected

    /// Reliable message queue - requeued on failure, drained first
    /// Contains already-serialized packets ready to write
    reliable_queue: VecDeque<Vec<u8>>,

    /// Unreliable message queue - dropped on failure, drained after reliable
    /// Contains already-serialized packets ready to write
    unreliable_queue: VecDeque<Vec<u8>>,

    /// Reconnection state management
    reconnect_state: ReconnectState,

    /// Metrics collection
    metrics: PeerMetrics,
}

impl<P: Providers> PeerSharedState<P> {
    /// Check if both message queues are empty.
    fn are_queues_empty(&self) -> bool {
        self.reliable_queue.is_empty() && self.unreliable_queue.is_empty()
    }
}

impl<P: Providers> Peer<P> {
    /// Create a new peer for the destination address.
    pub fn new(providers: P, destination: String, config: PeerConfig) -> Self {
        let reconnect_state = ReconnectState::new(config.initial_reconnect_delay);
        let now = providers.time().now();

        // Create shared state
        let shared_state = Rc::new(RefCell::new(PeerSharedState {
            network: providers.network().clone(),
            time: providers.time().clone(),
            destination,
            connection: None,
            reliable_queue: VecDeque::new(),
            unreliable_queue: VecDeque::new(),
            reconnect_state,
            metrics: PeerMetrics::new_at(now),
        }));

        // Create coordination primitives
        let data_to_send = Rc::new(Notify::new());
        let (receive_tx, receive_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();

        // Spawn background connection task
        let writer_handle = providers.task().spawn_task(
            "connection_task",
            connection_task(
                shared_state.clone(),
                data_to_send.clone(),
                config.clone(),
                receive_tx,
                shutdown_rx,
                None, // No initial stream - will establish connection
                ConnectionLossBehavior::Reconnect,
            ),
        );

        Self {
            shared_state,
            data_to_send,
            writer_handle: Some(writer_handle),
            receive_rx: Some(receive_rx),
            shutdown_tx,
            config,
            providers,
        }
    }

    /// Create a new peer with default configuration.
    pub fn new_with_defaults(providers: P, destination: String) -> Self {
        Self::new(providers, destination, PeerConfig::default())
    }

    /// Create a new peer from an incoming (already-connected) stream.
    ///
    /// FDB Pattern: `Peer::onIncomingConnection()` (FlowTransport.actor.cpp:1123)
    ///
    /// Used by server-side listener to wrap accepted connections.
    /// Unlike `new()`, this starts with an established connection instead of
    /// initiating an outbound connection.
    pub fn new_incoming(
        providers: P,
        peer_address: String,
        stream: <P::Network as moonpool_core::NetworkProvider>::TcpStream,
        config: PeerConfig,
    ) -> Self {
        let reconnect_state = ReconnectState::new(config.initial_reconnect_delay);
        let now = providers.time().now();

        // Create shared state - mark as connected since we have an existing stream
        let shared_state = Rc::new(RefCell::new(PeerSharedState {
            network: providers.network().clone(),
            time: providers.time().clone(),
            destination: peer_address,
            connection: Some(()), // Already connected
            reliable_queue: VecDeque::new(),
            unreliable_queue: VecDeque::new(),
            reconnect_state,
            metrics: PeerMetrics::new_at(now),
        }));

        // Mark metrics as connected
        shared_state.borrow_mut().metrics.is_connected = true;
        shared_state
            .borrow_mut()
            .metrics
            .record_connection_success_at(now);

        // Create coordination primitives
        let data_to_send = Rc::new(Notify::new());
        let (receive_tx, receive_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();

        // Spawn background connection task with the existing stream
        let writer_handle = providers.task().spawn_task(
            "incoming_connection_task",
            connection_task(
                shared_state.clone(),
                data_to_send.clone(),
                config.clone(),
                receive_tx,
                shutdown_rx,
                Some(stream),
                ConnectionLossBehavior::Exit,
            ),
        );

        Self {
            shared_state,
            data_to_send,
            writer_handle: Some(writer_handle),
            receive_rx: Some(receive_rx),
            shutdown_tx,
            config,
            providers,
        }
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.shared_state.borrow().connection.is_some()
    }

    /// Get current queue size (reliable + unreliable).
    pub fn queue_size(&self) -> usize {
        let state = self.shared_state.borrow();
        state.reliable_queue.len() + state.unreliable_queue.len()
    }

    /// Get reliable queue size.
    pub fn reliable_queue_size(&self) -> usize {
        self.shared_state.borrow().reliable_queue.len()
    }

    /// Get unreliable queue size.
    pub fn unreliable_queue_size(&self) -> usize {
        self.shared_state.borrow().unreliable_queue.len()
    }

    /// Get peer metrics.
    pub fn metrics(&self) -> PeerMetrics {
        self.shared_state.borrow().metrics.clone()
    }

    /// Get destination address.
    pub fn destination(&self) -> String {
        self.shared_state.borrow().destination.clone()
    }

    /// Send packet reliably to the peer (queued, will retry on reconnect).
    ///
    /// Serializes with wire format and queues immediately.
    /// Returns without blocking on TCP I/O (matches FoundationDB pattern).
    ///
    /// # Errors
    ///
    /// Returns error if payload is too large for wire format.
    pub fn send_reliable(&mut self, token: UID, payload: &[u8]) -> PeerResult<()> {
        let packet = Self::serialize_message(token, payload)?;

        tracing::debug!(
            "Peer::send_reliable called with token={}, payload={} bytes, packet={} bytes",
            token,
            payload.len(),
            packet.len()
        );

        // Queue serialized packet (FoundationDB pattern)
        {
            let mut state = self.shared_state.borrow_mut();

            // Check queue capacity before adding
            assert_sometimes!(
                state.reliable_queue.len() >= (self.config.max_queue_size as f64 * 0.8) as usize,
                "Message queue should sometimes approach capacity limit"
            );

            // Handle queue overflow
            if state.reliable_queue.len() >= self.config.max_queue_size
                && state.reliable_queue.pop_front().is_some()
            {
                state.metrics.record_message_dropped();
            }

            let first_unsent = state.are_queues_empty();
            state.reliable_queue.push_back(packet);
            state.metrics.record_message_queued();
            tracing::debug!(
                "Peer::send_reliable queued packet, reliable_queue size now: {}, first_unsent: {}",
                state.reliable_queue.len(),
                first_unsent
            );

            // Check if queue is growing with multiple messages
            assert_sometimes!(
                state.reliable_queue.len() > 1,
                "Message queue should sometimes contain multiple messages"
            );

            // Wake connection task if this is first message (FoundationDB pattern)
            if first_unsent {
                tracing::debug!("Peer::send_reliable notifying connection task to wake up");
                self.data_to_send.notify_one();
            } else {
                tracing::debug!(
                    "Peer::send_reliable NOT notifying connection task (queue was not empty)"
                );
            }
        }

        tracing::debug!("Peer::send_reliable completed successfully");
        Ok(())
    }

    /// Send packet unreliably (best-effort, dropped on connection failure).
    ///
    /// Queues the packet but does NOT retry on failure - unreliable packets
    /// are discarded when connection is lost (FDB pattern).
    ///
    /// # Errors
    ///
    /// Returns error if payload is too large for wire format.
    pub fn send_unreliable(&mut self, token: UID, payload: &[u8]) -> PeerResult<()> {
        let packet = Self::serialize_message(token, payload)?;

        tracing::debug!(
            "Peer::send_unreliable called with token={}, payload={} bytes",
            token,
            payload.len()
        );

        // Queue packet in unreliable queue (will be discarded on failure)
        {
            let mut state = self.shared_state.borrow_mut();
            let first_unsent = state.are_queues_empty();
            state.unreliable_queue.push_back(packet);
            state.metrics.record_message_queued();

            if first_unsent {
                self.data_to_send.notify_one();
            }
        }

        Ok(())
    }

    /// Serialize a message with wire format, mapping errors appropriately.
    fn serialize_message(token: UID, payload: &[u8]) -> PeerResult<Vec<u8>> {
        serialize_packet(token, payload).map_err(|e| match e {
            WireError::PacketTooLarge { size } => {
                PeerError::InvalidOperation(format!("payload too large: {} bytes", size))
            }
            _ => PeerError::InvalidOperation(format!("serialization error: {}", e)),
        })
    }

    /// Take ownership of the receive channel.
    ///
    /// This allows an external task to receive messages directly from the
    /// channel without borrowing the Peer. Useful for avoiding RefCell
    /// borrows across await points.
    ///
    /// After calling this, `receive()` and `try_receive()` will return
    /// `PeerError::ReceiverTaken`.
    ///
    /// # Returns
    ///
    /// `Some(receiver)` if the receiver hasn't been taken yet, `None` otherwise.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut peer = Peer::new(...);
    /// let receiver = peer.take_receiver().expect("receiver not yet taken");
    ///
    /// // Now can await on receiver without borrowing peer
    /// loop {
    ///     match receiver.recv().await {
    ///         Some((token, payload)) => { /* handle message */ }
    ///         None => break, // Channel closed
    ///     }
    /// }
    /// ```
    pub fn take_receiver(&mut self) -> Option<PeerReceiver> {
        self.receive_rx.take()
    }

    /// Check if the receiver has been taken.
    pub fn receiver_taken(&self) -> bool {
        self.receive_rx.is_none()
    }

    /// Receive packet from the peer.
    ///
    /// Returns the endpoint token and payload bytes.
    /// Waits for data from the background reader actor.
    ///
    /// # Errors
    ///
    /// Returns `PeerError::ReceiverTaken` if `take_receiver()` was called.
    /// Returns `PeerError::Disconnected` if the peer connection is closed.
    pub async fn receive(&mut self) -> PeerResult<(UID, Vec<u8>)> {
        match &mut self.receive_rx {
            Some(rx) => rx.recv().await.ok_or(PeerError::Disconnected),
            None => Err(PeerError::ReceiverTaken),
        }
    }

    /// Try to receive packet from the peer without blocking.
    ///
    /// Returns immediately with (token, payload) if available.
    ///
    /// # Errors
    ///
    /// Returns `Err(PeerError::ReceiverTaken)` if `take_receiver()` was called.
    /// Returns `Ok(None)` if no message is currently available.
    pub fn try_receive(&mut self) -> PeerResult<Option<(UID, Vec<u8>)>> {
        match &mut self.receive_rx {
            Some(rx) => Ok(rx.try_recv().ok()),
            None => Err(PeerError::ReceiverTaken),
        }
    }

    /// Force reconnection by dropping current connection.
    pub fn reconnect(&mut self) {
        let mut state = self.shared_state.borrow_mut();
        state.connection = None;
        state.metrics.is_connected = false;
        state
            .reconnect_state
            .reset(self.config.initial_reconnect_delay);

        // Connection task will automatically attempt reconnection
        self.data_to_send.notify_one();
    }

    /// Close the connection and clear send queues.
    pub async fn close(&mut self) {
        // Signal shutdown to connection task
        let _ = self.shutdown_tx.send(());

        // Wait for connection task to complete
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.await;
        }

        // Clear state
        let mut state = self.shared_state.borrow_mut();
        state.connection = None;
        state.reliable_queue.clear();
        state.unreliable_queue.clear();
        state.metrics.is_connected = false;
        state.metrics.current_queue_size = 0;
    }
}

// =============================================================================
// Connection Task (Background Actor)
// =============================================================================

/// Behavior when connection is lost.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionLossBehavior {
    /// Attempt to reconnect (outbound connections)
    Reconnect,
    /// Exit the task (incoming connections - client must reconnect)
    Exit,
}

/// Background connection task that handles all async TCP I/O.
///
/// Matches FoundationDB's connectionWriter + connectionMonitor patterns:
/// - Waits for dataToSend trigger
/// - Drains unsent queue continuously
/// - Handles connection failures and reconnection (or exit for incoming)
/// - Owns the connection exclusively to avoid RefCell conflicts
/// - Handles both reading and writing operations
/// - Parses wire format packets from the read stream
/// - Periodically pings to detect unresponsive connections (when monitoring enabled)
async fn connection_task<P: Providers>(
    shared_state: Rc<RefCell<PeerSharedState<P>>>,
    data_to_send: Rc<Notify>,
    config: PeerConfig,
    receive_tx: mpsc::UnboundedSender<(UID, Vec<u8>)>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
    initial_stream: Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    on_connection_loss: ConnectionLossBehavior,
) {
    let mut current_connection: Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream> =
        initial_stream;
    // Buffer for accumulating partial packet reads
    let mut read_buffer: Vec<u8> = Vec::with_capacity(4096);

    // Initialize ping tracker: only for outbound peers with monitoring enabled
    let mut ping_tracker: Option<PingTracker> = match (&config.monitor, on_connection_loss) {
        (Some(monitor_config), ConnectionLossBehavior::Reconnect) => {
            Some(PingTracker::new(monitor_config.clone()))
        }
        _ => None,
    };

    // If we start with an existing connection, initialize the ping cycle
    if current_connection.is_some()
        && let Some(ref mut tracker) = ping_tracker
    {
        let now = shared_state.borrow().time.now();
        tracker.last_ping_cycle = Some(now);
    }

    loop {
        // Compute ping timer duration before select! to avoid borrow conflicts
        let ping_sleep_duration = if let Some(ref tracker) = ping_tracker {
            if current_connection.is_some() {
                let now = shared_state.borrow().time.now();
                Some(tracker.time_until_next_action(now))
            } else {
                None
            }
        } else {
            None
        };
        let ping_active = ping_sleep_duration.is_some();

        tokio::select! {
            // Check for shutdown
            _ = shutdown_rx.recv() => {
                break;
            }

            // Wait for data to send (FoundationDB pattern)
            _ = data_to_send.notified() => {
                // First, ensure we have messages to send
                let has_messages = {
                    let state = shared_state.borrow();
                    let total = state.reliable_queue.len() + state.unreliable_queue.len();
                    tracing::debug!("connection_task: queues have {} messages (reliable={}, unreliable={})",
                        total, state.reliable_queue.len(), state.unreliable_queue.len());
                    total > 0
                };

                if !has_messages {
                    tracing::debug!("connection_task: spurious wakeup, no messages to send");
                    continue; // Spurious wakeup, wait for real data
                }

                // Ensure we have a connection
                if current_connection.is_none() {
                    match on_connection_loss {
                        ConnectionLossBehavior::Reconnect => {
                            tracing::debug!("connection_task: no connection, establishing new connection");
                            match establish_connection(&shared_state, &config).await {
                                Ok(stream) => {
                                    tracing::debug!("connection_task: successfully established connection");
                                    current_connection = Some(stream);
                                    read_buffer.clear();
                                    {
                                        let mut state = shared_state.borrow_mut();
                                        state.connection = Some(());
                                        state.metrics.is_connected = true;
                                    }
                                    // Reset ping tracker on new connection
                                    if let Some(ref mut tracker) = ping_tracker {
                                        let now = shared_state.borrow().time.now();
                                        tracker.reset();
                                        tracker.last_ping_cycle = Some(now);
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("connection_task: failed to establish connection: {:?}", e);
                                    continue; // Will retry on next notification
                                }
                            }
                        }
                        ConnectionLossBehavior::Exit => {
                            tracing::debug!("connection_task: connection lost, exiting (client must reconnect)");
                            break;
                        }
                    }
                }

                // Process send queues - drain reliable first, then unreliable (FDB pattern)
                tracing::debug!("connection_task: processing send queues");
                while let Some(ref mut stream) = current_connection {
                    // Get next message - reliable queue has priority
                    let (message, is_reliable) = {
                        let mut state = shared_state.borrow_mut();
                        if let Some(msg) = state.reliable_queue.pop_front() {
                            tracing::debug!("connection_task: popped reliable message, remaining: {}",
                                state.reliable_queue.len());
                            (Some(msg), true)
                        } else if let Some(msg) = state.unreliable_queue.pop_front() {
                            tracing::debug!("connection_task: popped unreliable message, remaining: {}",
                                state.unreliable_queue.len());
                            (Some(msg), false)
                        } else {
                            (None, false)
                        }
                    };

                    let Some(data) = message else {
                        tracing::debug!("connection_task: all queues empty, breaking");
                        break; // All queues empty
                    };

                    tracing::debug!("connection_task: attempting to send {} bytes (reliable={})",
                        data.len(), is_reliable);

                    // Buggify: Sometimes force write failures to test requeuing
                    if moonpool_sim::buggify_with_prob!(0.02) {
                        tracing::debug!("Buggify forcing write failure for requeue testing");
                        assert_sometimes!(true, "buggified_write_failure");
                        handle_connection_failure(
                            &shared_state,
                            &mut current_connection,
                            &mut read_buffer,
                            Some((data, is_reliable)),
                        );
                        if let Some(ref mut tracker) = ping_tracker {
                            tracker.reset();
                        }
                        break; // Exit send loop, will retry on next trigger
                    }

                    // Send the message (no RefCell borrow held)
                    tracing::debug!("connection_task: calling stream.write_all() with {} bytes", data.len());
                    match stream.write_all(&data).await {
                        Ok(_) => {
                            tracing::debug!("connection_task: write_all succeeded");
                            {
                                let mut state = shared_state.borrow_mut();
                                state.metrics.record_message_sent(data.len());
                                state.metrics.record_message_dequeued();
                            }
                        }
                        Err(e) => {
                            tracing::debug!("connection_task: write_all failed: {:?}", e);
                            handle_connection_failure(
                                &shared_state,
                                &mut current_connection,
                                &mut read_buffer,
                                Some((data, is_reliable)),
                            );
                            if let Some(ref mut tracker) = ping_tracker {
                                tracker.reset();
                            }
                            break; // Exit send loop, will retry on next trigger
                        }
                    }
                }
            }

            // Handle reading (truly event-driven - FDB pattern)
            read_result = async {
                match &mut current_connection {
                    Some(stream) => {
                        let mut buffer = vec![0u8; 4096];
                        stream.read(&mut buffer).await.map(|n| (buffer, n))
                    }
                    None => std::future::pending().await  // Never resolves when no connection
                }
            } => {
                match read_result {
                    Ok((_buffer, 0)) => {
                        // Connection closed
                        assert_sometimes!(true, "graceful_close_on_read");
                        handle_connection_failure(
                            &shared_state,
                            &mut current_connection,
                            &mut read_buffer,
                            None,
                        );
                        if let Some(ref mut tracker) = ping_tracker {
                            tracker.reset();
                        }
                        if on_connection_loss == ConnectionLossBehavior::Exit {
                            break;
                        }
                    }
                    Ok((buffer, n)) => {
                        // Append to read buffer
                        read_buffer.extend_from_slice(&buffer[..n]);
                        tracing::debug!("connection_task: received {} bytes, buffer now {} bytes", n, read_buffer.len());

                        // Try to parse complete packets from buffer
                        // Pong replies are enqueued in unreliable_queue by process_read_buffer
                        // and picked up by the writer branch (FDB pattern: connectionWriter is sole TCP writer)
                        let should_exit = process_read_buffer(
                            &shared_state,
                            &mut current_connection,
                            &mut read_buffer,
                            &receive_tx,
                            on_connection_loss,
                            &mut ping_tracker,
                            &data_to_send,
                        );

                        if should_exit {
                            break;
                        }
                    }
                    Err(_) => {
                        // Read error - connection likely broken
                        handle_connection_failure(
                            &shared_state,
                            &mut current_connection,
                            &mut read_buffer,
                            None,
                        );
                        if let Some(ref mut tracker) = ping_tracker {
                            tracker.reset();
                        }
                        if on_connection_loss == ConnectionLossBehavior::Exit {
                            break;
                        }
                    }
                }
            }

            // Connection monitor: periodic ping to detect unresponsive connections
            // FDB ref: connectionMonitor (FlowTransport.actor.cpp:616-699)
            _ = async {
                match ping_sleep_duration {
                    Some(duration) => {
                        let time = shared_state.borrow().time.clone();
                        let _ = time.sleep(duration).await;
                    }
                    None => std::future::pending::<()>().await,
                }
            }, if ping_active => {
                let (now, bytes_received) = {
                    let state = shared_state.borrow();
                    (state.time.now(), state.metrics.bytes_received)
                };

                if let Some(ref mut tracker) = ping_tracker {
                    match tracker.on_timer_fired(now, bytes_received) {
                        PingAction::SendPing => {
                            // Buggify: occasionally skip sending ping to test timeout path
                            let skip_ping = moonpool_sim::buggify_with_prob!(0.05);
                            if skip_ping {
                                assert_sometimes!(true, "buggified_ping_skip");
                                tracing::debug!("connection_task: buggify skipped ping send");
                            } else if current_connection.is_some()
                                && let Ok(ping_packet) = serialize_packet(PING_TOKEN, &[])
                            {
                                // FDB pattern: enqueue ping via unreliable queue, writer sends it
                                // (FlowTransport.actor.cpp:663 — connectionMonitor uses sendUnreliable)
                                let mut state = shared_state.borrow_mut();
                                let first_unsent = state.are_queues_empty();
                                state.unreliable_queue.push_back(ping_packet);
                                state.metrics.record_ping_sent();
                                drop(state);
                                if first_unsent {
                                    data_to_send.notify_one();
                                }
                                tracing::debug!("connection_task: enqueued ping in unreliable_queue");
                            }
                        }
                        PingAction::Tolerate => {
                            assert_sometimes!(true, "ping_timeout_tolerated");
                            shared_state
                                .borrow_mut()
                                .metrics
                                .record_ping_timeout_tolerated();
                            tracing::debug!(
                                "connection_task: ping timeout tolerated (bytes still flowing), count={}",
                                tracker.timeout_count
                            );
                            // Re-send a ping for the next timeout window via unreliable queue
                            if current_connection.is_some()
                                && let Ok(ping_packet) = serialize_packet(PING_TOKEN, &[])
                            {
                                let mut state = shared_state.borrow_mut();
                                let first_unsent = state.are_queues_empty();
                                state.unreliable_queue.push_back(ping_packet);
                                state.metrics.record_ping_sent();
                                let sent_at = state.time.now();
                                drop(state);
                                tracker.ping_sent_at = Some(sent_at);
                                if first_unsent {
                                    data_to_send.notify_one();
                                }
                            }
                        }
                        PingAction::TearDown => {
                            assert_sometimes!(true, "ping_timeout_teardown");
                            shared_state.borrow_mut().metrics.record_ping_timeout();
                            tracing::debug!(
                                "connection_task: ping timeout, tearing down connection to {}",
                                shared_state.borrow().destination
                            );
                            tracker.reset();
                            handle_connection_failure(
                                &shared_state,
                                &mut current_connection,
                                &mut read_buffer,
                                None,
                            );
                            if on_connection_loss == ConnectionLossBehavior::Exit {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}

// =============================================================================
// Connection Helpers
// =============================================================================

/// Handle connection failure by clearing state and optionally requeuing data.
fn handle_connection_failure<P: Providers>(
    shared_state: &Rc<RefCell<PeerSharedState<P>>>,
    current_connection: &mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &mut Vec<u8>,
    failed_send: Option<(Vec<u8>, bool)>, // (data, is_reliable)
) {
    *current_connection = None;
    read_buffer.clear();

    let mut state = shared_state.borrow_mut();
    state.connection = None;
    state.metrics.is_connected = false;

    // Handle the failed send if provided
    if let Some((data, is_reliable)) = failed_send {
        if is_reliable {
            assert_sometimes!(
                true,
                "Peer should sometimes re-queue reliable messages after send failure"
            );
            state.reliable_queue.push_front(data);
        } else {
            state.metrics.record_message_dropped();
        }
    }

    // Discard all remaining unreliable packets (FDB pattern: discardUnreliablePackets)
    let unreliable_count = state.unreliable_queue.len();
    if unreliable_count > 0 {
        tracing::debug!(
            "connection_task: discarding {} unreliable packets on failure",
            unreliable_count
        );
        for _ in 0..unreliable_count {
            state.metrics.record_message_dropped();
        }
        state.unreliable_queue.clear();
    }
}

/// Process the read buffer and parse packets.
///
/// Intercepts ping/pong tokens:
/// - `PING_TOKEN`: enqueues pong in `unreliable_queue` (FDB pattern: writer sends it)
/// - `PONG_TOKEN`: updates `ping_tracker` with RTT measurement
///
/// Returns true if the task should exit.
fn process_read_buffer<P: Providers>(
    shared_state: &Rc<RefCell<PeerSharedState<P>>>,
    current_connection: &mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &mut Vec<u8>,
    receive_tx: &mpsc::UnboundedSender<(UID, Vec<u8>)>,
    on_connection_loss: ConnectionLossBehavior,
    ping_tracker: &mut Option<PingTracker>,
    data_to_send: &Notify,
) -> bool {
    loop {
        if read_buffer.len() < HEADER_SIZE {
            return false; // Need more data for header
        }

        match try_deserialize_packet(read_buffer) {
            Ok(Some((token, payload, consumed))) => {
                tracing::debug!(
                    "connection_task: parsed packet token={}, payload={} bytes, consumed={} bytes",
                    token,
                    payload.len(),
                    consumed
                );

                {
                    let mut state = shared_state.borrow_mut();
                    state.metrics.record_message_received(consumed);
                }

                // Remove consumed bytes from buffer
                read_buffer.drain(..consumed);

                // Intercept ping token — enqueue pong in unreliable queue
                // FDB pattern: pong goes through sendPacket → unsent queue → connectionWriter
                if token == PING_TOKEN {
                    if let Ok(pong_packet) = serialize_packet(PONG_TOKEN, &[]) {
                        let mut state = shared_state.borrow_mut();
                        let first_unsent = state.are_queues_empty();
                        state.unreliable_queue.push_back(pong_packet);
                        drop(state);
                        if first_unsent {
                            data_to_send.notify_one();
                        }
                    }
                    tracing::debug!(
                        "connection_task: received ping, enqueued pong in unreliable_queue"
                    );
                    continue; // Do NOT deliver to application
                }

                // Intercept pong token — update ping tracker with RTT
                if token == PONG_TOKEN {
                    if let Some(tracker) = ping_tracker.as_mut() {
                        let now = shared_state.borrow().time.now();
                        if let Some(rtt) = tracker.on_pong_received(now) {
                            shared_state.borrow_mut().metrics.record_pong_received(rtt);
                            tracing::debug!("connection_task: received pong, rtt={:?}", rtt);
                            assert_sometimes!(true, "pong_received_with_rtt");
                        }
                    }
                    continue; // Do NOT deliver to application
                }

                // Normal packet — deliver to application
                if receive_tx.send((token, payload)).is_err() {
                    return true; // Receiver dropped, exit
                }
            }
            Ok(None) => {
                return false; // Need more data
            }
            Err(e) => {
                // Protocol error - invalid packet
                // FDB pattern: Tear down connection on wire errors
                use crate::WireError;
                match &e {
                    WireError::ChecksumMismatch { expected, actual } => {
                        tracing::warn!(
                            "ChecksumMismatch: expected={:#010x} actual={:#010x} - tearing down connection (FDB pattern)",
                            expected,
                            actual
                        );
                        assert_sometimes!(
                            true,
                            "Checksum validation should sometimes catch corrupted packets"
                        );
                    }
                    _ => {
                        tracing::warn!(
                            "connection_task: wire format error: {} - tearing down connection",
                            e
                        );
                    }
                }

                assert_sometimes!(
                    true,
                    "Connection should sometimes be torn down on wire format errors"
                );

                *current_connection = None;
                read_buffer.clear();

                // Reset ping tracker on wire error
                if let Some(tracker) = ping_tracker.as_mut() {
                    tracker.reset();
                }

                {
                    let mut state = shared_state.borrow_mut();
                    state.connection = None;
                    state.metrics.is_connected = false;

                    let unreliable_count = state.unreliable_queue.len();
                    if unreliable_count > 0 {
                        tracing::debug!(
                            "connection_task: discarding {} unreliable packets after wire error (FDB pattern)",
                            unreliable_count
                        );
                        assert_sometimes!(
                            true,
                            "Unreliable packets should sometimes be discarded on connection errors"
                        );
                        for _ in 0..unreliable_count {
                            state.metrics.record_message_dropped();
                        }
                        state.unreliable_queue.clear();
                    }
                }

                return on_connection_loss == ConnectionLossBehavior::Exit;
            }
        }
    }
}

/// Establish a connection with exponential backoff.
async fn establish_connection<P: Providers>(
    shared_state: &Rc<RefCell<PeerSharedState<P>>>,
    config: &PeerConfig,
) -> PeerResult<<P::Network as moonpool_core::NetworkProvider>::TcpStream> {
    loop {
        // Check failure limits and get connection params
        let (network, time, destination, should_backoff, delay) = {
            let state = shared_state.borrow_mut();

            // Check failure limits
            if let Some(max_failures) = config.max_connection_failures
                && state.reconnect_state.failure_count >= max_failures
            {
                assert_sometimes!(true, "max_failures_reached");
                return Err(PeerError::ConnectionFailed);
            }

            // Check if backoff is needed
            let now = state.time.now();
            let (should_backoff, delay) =
                if let Some(last_attempt) = state.reconnect_state.last_attempt {
                    let elapsed = now.saturating_sub(last_attempt);
                    if elapsed < state.reconnect_state.current_delay {
                        (true, state.reconnect_state.current_delay - elapsed)
                    } else {
                        (false, Duration::from_secs(0))
                    }
                } else {
                    (false, Duration::from_secs(0))
                };

            (
                state.network.clone(),
                state.time.clone(),
                state.destination.clone(),
                should_backoff,
                delay,
            )
        };

        // Apply backoff if needed (no RefCell borrow held)
        if should_backoff && time.sleep(delay).await.is_err() {
            return Err(PeerError::ConnectionFailed);
        }

        // Record attempt
        {
            let mut state = shared_state.borrow_mut();
            state.reconnect_state.last_attempt = Some(state.time.now());
            state.metrics.record_connection_attempt();
        }

        // Attempt connection (no RefCell borrow held)
        match time
            .timeout(config.connection_timeout, network.connect(&destination))
            .await
        {
            Ok(Ok(stream)) => {
                // Success - check if this was a recovery after failures
                {
                    let mut state = shared_state.borrow_mut();

                    if state.reconnect_state.failure_count > 0 {
                        assert_sometimes!(
                            true,
                            "Peer should sometimes successfully connect after previous failures"
                        );
                    }

                    let now = state.time.now();
                    state.connection = Some(()); // Mark as connected
                    state.reconnect_state.reset(config.initial_reconnect_delay);
                    state.metrics.record_connection_success_at(now);
                    state.metrics.is_connected = true;
                }
                return Ok(stream);
            }
            Ok(Err(_)) => {
                // Connection refused/failed - update state and retry
                record_connection_failure(shared_state, config);
                // Continue loop to retry
            }
            Err(_) => {
                // Connection attempt timed out
                assert_sometimes!(true, "connection_timed_out");
                record_connection_failure(shared_state, config);
                // Continue loop to retry
            }
        }
    }
}

/// Record a connection failure: increment failure count, update backoff delay, record metrics.
fn record_connection_failure<P: Providers>(
    shared_state: &Rc<RefCell<PeerSharedState<P>>>,
    config: &PeerConfig,
) {
    let mut state = shared_state.borrow_mut();
    state.reconnect_state.failure_count += 1;
    assert_sometimes_each!(
        "backoff_depth",
        [("attempt", state.reconnect_state.failure_count)]
    );
    let next_delay = std::cmp::min(
        state.reconnect_state.current_delay * 2,
        config.max_reconnect_delay,
    );
    state.reconnect_state.current_delay = next_delay;
    let now = state.time.now();
    state.metrics.record_connection_failure_at(now, next_delay);
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ping_tracker_idle_sends_ping() {
        let config = MonitorConfig {
            ping_interval: Duration::from_secs(1),
            ping_timeout: Duration::from_secs(2),
            max_tolerated_timeouts: 3,
        };
        let mut tracker = PingTracker::new(config);

        let action = tracker.on_timer_fired(Duration::from_secs(1), 0);
        assert!(matches!(action, PingAction::SendPing));
        assert!(tracker.ping_sent_at.is_some());
        assert_eq!(tracker.timeout_count, 0);
    }

    #[test]
    fn test_ping_tracker_pong_records_rtt() {
        let config = MonitorConfig::default();
        let mut tracker = PingTracker::new(config);

        // Send ping at t=1s
        tracker.on_timer_fired(Duration::from_secs(1), 0);
        assert!(tracker.ping_sent_at.is_some());

        // Receive pong at t=1.5s
        let rtt = tracker.on_pong_received(Duration::from_millis(1500));
        assert_eq!(rtt, Some(Duration::from_millis(500)));
        assert!(tracker.ping_sent_at.is_none()); // Back to idle
        assert_eq!(tracker.timeout_count, 0);
    }

    #[test]
    fn test_ping_tracker_timeout_with_bytes_tolerates() {
        let config = MonitorConfig {
            ping_interval: Duration::from_secs(1),
            ping_timeout: Duration::from_secs(2),
            max_tolerated_timeouts: 3,
        };
        let mut tracker = PingTracker::new(config);

        // Send ping at t=0, with 100 bytes received
        tracker.on_timer_fired(Duration::ZERO, 100);
        assert!(tracker.ping_sent_at.is_some());

        // Timeout at t=2, but 200 bytes now (increased)
        let action = tracker.on_timer_fired(Duration::from_secs(2), 200);
        assert!(matches!(action, PingAction::Tolerate));
        assert_eq!(tracker.timeout_count, 1);
    }

    #[test]
    fn test_ping_tracker_timeout_without_bytes_tears_down() {
        let config = MonitorConfig::default();
        let mut tracker = PingTracker::new(config);

        // Send ping at t=0
        tracker.on_timer_fired(Duration::ZERO, 100);

        // Timeout with same bytes — connection dead
        let action = tracker.on_timer_fired(Duration::from_secs(2), 100);
        assert!(matches!(action, PingAction::TearDown));
    }

    #[test]
    fn test_ping_tracker_max_tolerated_timeouts() {
        let config = MonitorConfig {
            ping_interval: Duration::from_secs(1),
            ping_timeout: Duration::from_secs(2),
            max_tolerated_timeouts: 2,
        };
        let mut tracker = PingTracker::new(config);

        // Send ping
        tracker.on_timer_fired(Duration::ZERO, 0);

        // First timeout with bytes — tolerate
        let action = tracker.on_timer_fired(Duration::from_secs(2), 10);
        assert!(matches!(action, PingAction::Tolerate));

        // Second timeout with bytes — tolerate
        let action = tracker.on_timer_fired(Duration::from_secs(4), 20);
        assert!(matches!(action, PingAction::Tolerate));

        // Third timeout with bytes — exceeds max_tolerated (2), tear down
        let action = tracker.on_timer_fired(Duration::from_secs(6), 30);
        assert!(matches!(action, PingAction::TearDown));
    }

    #[test]
    fn test_ping_tracker_reset() {
        let config = MonitorConfig::default();
        let mut tracker = PingTracker::new(config);

        // Set some state
        tracker.on_timer_fired(Duration::from_secs(1), 100);
        assert!(tracker.ping_sent_at.is_some());

        // Reset
        tracker.reset();
        assert!(tracker.ping_sent_at.is_none());
        assert!(tracker.last_ping_cycle.is_none());
        assert_eq!(tracker.bytes_at_ping, 0);
        assert_eq!(tracker.timeout_count, 0);
    }

    #[test]
    fn test_ping_tracker_time_until_next_action_idle() {
        let config = MonitorConfig {
            ping_interval: Duration::from_secs(5),
            ping_timeout: Duration::from_secs(2),
            max_tolerated_timeouts: 3,
        };
        let mut tracker = PingTracker::new(config);
        tracker.last_ping_cycle = Some(Duration::from_secs(10));

        // 3 seconds after last cycle, should wait 2 more seconds
        let wait = tracker.time_until_next_action(Duration::from_secs(13));
        assert_eq!(wait, Duration::from_secs(2));
    }

    #[test]
    fn test_ping_tracker_time_until_next_action_awaiting_pong() {
        let config = MonitorConfig {
            ping_interval: Duration::from_secs(5),
            ping_timeout: Duration::from_secs(3),
            max_tolerated_timeouts: 3,
        };
        let mut tracker = PingTracker::new(config);

        // Send ping at t=10
        tracker.on_timer_fired(Duration::from_secs(10), 0);

        // At t=11, should wait 2 more seconds for timeout
        let wait = tracker.time_until_next_action(Duration::from_secs(11));
        assert_eq!(wait, Duration::from_secs(2));
    }

    #[test]
    fn test_ping_tracker_spurious_pong() {
        let config = MonitorConfig::default();
        let mut tracker = PingTracker::new(config);

        // Receive pong without sending ping — should return None
        let rtt = tracker.on_pong_received(Duration::from_secs(1));
        assert!(rtt.is_none());
    }

    #[test]
    fn test_ping_pong_tokens_distinct() {
        assert_ne!(PING_TOKEN, PONG_TOKEN);
        // PING_TOKEN is well-known
        assert!(PING_TOKEN.is_well_known());
        // PONG_TOKEN is NOT well-known (avoids endpoint dispatch)
        assert!(!PONG_TOKEN.is_well_known());
    }
}
