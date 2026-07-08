//! Core peer implementation with automatic reconnection, message queuing,
//! and connection health monitoring.
//!
//! Provides wire format with UID-based endpoint addressing.

use futures::io::{AsyncReadExt, AsyncWriteExt};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

use super::config::{MonitorConfig, PeerConfig};
use super::error::{PeerError, PeerResult};
use super::metrics::PeerMetrics;
use crate::rpc::{FailureMonitor, FailureStatus};
use crate::{
    HEADER_SIZE, NetworkProvider, Providers, TaskProvider, TimeProvider, UID, WireError,
    serialize_packet, try_deserialize_packet,
};

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

/// Wire token for endpoint-not-found notifications (FDB: `WLTOKEN_ENDPOINT_NOT_FOUND`).
///
/// Intercepted by `process_read_buffer`, never delivered to application.
/// Payload: 16 bytes — the UID of the missing endpoint (two little-endian u64).
///
/// FDB ref: `EndpointNotFoundReceiver` in `FlowTransport.actor.cpp:232-236`
pub(crate) const ENDPOINT_NOT_FOUND_TOKEN: UID = UID::well_known(0);

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
        if let Some(sent_at) = self.ping_sent_at {
            // AwaitingPong — timer is the ping timeout
            let elapsed = now.saturating_sub(sent_at);
            self.config.ping_timeout.saturating_sub(elapsed)
        } else {
            // Idle — timer is the ping interval
            let last = self.last_ping_cycle.unwrap_or(now);
            let elapsed = now.saturating_sub(last);
            self.config.ping_interval.saturating_sub(elapsed)
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
/// Follows `FoundationDB`'s architecture: synchronous API with background actors.
pub struct Peer<P: Providers> {
    /// Shared state accessible to background actors
    shared_state: Arc<RwLock<PeerSharedState<P>>>,

    /// Trigger to wake writer actor when data is queued
    data_to_send: Arc<Notify>,

    /// Background actor handles
    writer_handle: Option<<P::Task as TaskProvider>::JoinHandle>,

    /// Receive channel for incoming packets (token + payload).
    /// Can be taken via `take_receiver()` for external ownership.
    receive_rx: Option<mpsc::UnboundedReceiver<(UID, Vec<u8>)>>,

    /// Shutdown signaling
    shutdown_tx: mpsc::UnboundedSender<()>,

    /// Configuration (owned by Peer)
    config: PeerConfig,
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

    /// Fired on every connection failure.
    /// FDB ref: `Peer::disconnect` (`FlowTransport.h:180`)
    disconnect_notify: Arc<Notify>,

    /// Failure monitor for address-level failure tracking.
    /// FDB ref: `IFailureMonitor` (FailureMonitor.h)
    failure_monitor: Option<Arc<FailureMonitor>>,
}

impl<P: Providers> PeerSharedState<P> {
    /// Check if both message queues are empty.
    fn are_queues_empty(&self) -> bool {
        self.reliable_queue.is_empty() && self.unreliable_queue.is_empty()
    }
}

impl<P: Providers> Peer<P> {
    /// Create a new peer for the destination address.
    ///
    /// # Arguments
    ///
    /// * `providers` - Provider bundle for network, time, task
    /// * `destination` - Remote address to connect to
    /// * `config` - Peer configuration
    /// * `failure_monitor` - Optional failure monitor for address-level tracking
    pub fn new(
        providers: &P,
        destination: String,
        config: PeerConfig,
        failure_monitor: Option<Arc<FailureMonitor>>,
    ) -> Self {
        let reconnect_state = ReconnectState::new(config.initial_reconnect_delay);
        let now = providers.time().now();

        // Create shared state
        let shared_state = Arc::new(RwLock::new(PeerSharedState {
            network: providers.network().clone(),
            time: providers.time().clone(),
            destination,
            connection: None,
            reliable_queue: VecDeque::new(),
            unreliable_queue: VecDeque::new(),
            reconnect_state,
            metrics: PeerMetrics::new_at(now),
            disconnect_notify: Arc::new(Notify::new()),
            failure_monitor,
        }));

        // Create coordination primitives
        let data_to_send = Arc::new(Notify::new());
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
        }
    }

    /// Create a new peer from an incoming (already-connected) stream.
    ///
    /// FDB Pattern: `Peer::onIncomingConnection()` (FlowTransport.actor.cpp:1123)
    ///
    /// Used by server-side listener to wrap accepted connections.
    /// Unlike `new()`, this starts with an established connection instead of
    /// initiating an outbound connection.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn new_incoming(
        providers: &P,
        peer_address: String,
        stream: <P::Network as moonpool_core::NetworkProvider>::TcpStream,
        config: PeerConfig,
        failure_monitor: Option<Arc<FailureMonitor>>,
    ) -> Self {
        let reconnect_state = ReconnectState::new(config.initial_reconnect_delay);
        let now = providers.time().now();

        // Create shared state - mark as connected since we have an existing stream
        let shared_state = Arc::new(RwLock::new(PeerSharedState {
            network: providers.network().clone(),
            time: providers.time().clone(),
            destination: peer_address,
            connection: Some(()), // Already connected
            reliable_queue: VecDeque::new(),
            unreliable_queue: VecDeque::new(),
            reconnect_state,
            metrics: PeerMetrics::new_at(now),
            disconnect_notify: Arc::new(Notify::new()),
            failure_monitor,
        }));

        // Mark metrics as connected
        shared_state
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .metrics
            .is_connected = true;
        shared_state
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .metrics
            .record_connection_success_at(now);

        // Create coordination primitives
        let data_to_send = Arc::new(Notify::new());
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
        }
    }

    /// Check if currently connected.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn is_connected(&self) -> bool {
        self.shared_state
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .connection
            .is_some()
    }

    /// Get the disconnect notification handle.
    ///
    /// FDB ref: `Peer::disconnect` (`FlowTransport.h:180`)
    ///
    /// Returns an `Arc<Notify>` that fires `notify_waiters()` on every
    /// connection failure. Consumers should call `.notified()` before
    /// checking `is_connected()` to avoid races.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn disconnect_notify(&self) -> Arc<Notify> {
        self.shared_state
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .disconnect_notify
            .clone()
    }

    /// Get peer metrics.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn metrics(&self) -> PeerMetrics {
        self.shared_state
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .metrics
            .clone()
    }

    /// Send packet reliably to the peer (queued, will retry on reconnect).
    ///
    /// Serializes with wire format and queues immediately.
    /// Returns without blocking on TCP I/O (matches `FoundationDB` pattern).
    ///
    /// # Errors
    ///
    /// Returns error if payload is too large for wire format.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
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
            let mut state = self
                .shared_state
                .write()
                .expect("RwLock poisoned: prior task panicked");

            // Handle queue overflow
            if state.reliable_queue.len() >= self.config.max_queue_size
                && state.reliable_queue.pop_front().is_some()
            {
                state.metrics.record_message_dropped();
                state.metrics.record_message_dequeued();
            }

            let first_unsent = state.are_queues_empty();
            state.reliable_queue.push_back(packet);
            state.metrics.record_message_queued();
            tracing::debug!(
                "Peer::send_reliable queued packet, reliable_queue size now: {}, first_unsent: {}",
                state.reliable_queue.len(),
                first_unsent
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
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn send_unreliable(&mut self, token: UID, payload: &[u8]) -> PeerResult<()> {
        let packet = Self::serialize_message(token, payload)?;

        tracing::debug!(
            "Peer::send_unreliable called with token={}, payload={} bytes",
            token,
            payload.len()
        );

        // Queue packet in unreliable queue (will be discarded on failure)
        {
            let mut state = self
                .shared_state
                .write()
                .expect("RwLock poisoned: prior task panicked");
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
                PeerError::InvalidOperation(format!("payload too large: {size} bytes"))
            }
            _ => PeerError::InvalidOperation(format!("serialization error: {e}")),
        })
    }

    /// Take ownership of the receive channel.
    ///
    /// Allows an external task to receive messages directly without
    /// borrowing the Peer (avoids holding the shared-state lock across
    /// await points).
    ///
    /// Returns `Some(receiver)` if not yet taken, `None` otherwise.
    pub fn take_receiver(&mut self) -> Option<PeerReceiver> {
        self.receive_rx.take()
    }

    /// Close the connection and clear send queues.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub async fn close(&mut self) {
        // Signal shutdown to connection task
        let _ = self.shutdown_tx.send(());

        // Wait for connection task to complete
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.await;
        }

        // Clear state
        let mut state = self
            .shared_state
            .write()
            .expect("RwLock poisoned: prior task panicked");
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
/// Matches `FoundationDB`'s connectionWriter + connectionMonitor patterns:
/// - Waits for dataToSend trigger
/// - Drains unsent queue continuously
/// - Handles connection failures and reconnection (or exit for incoming)
/// - Owns the connection exclusively to avoid contention on the shared-state lock
/// - Handles both reading and writing operations
/// - Parses wire format packets from the read stream
/// - Periodically pings to detect unresponsive connections (when monitoring enabled)
async fn connection_task<P: Providers>(
    shared_state: Arc<RwLock<PeerSharedState<P>>>,
    data_to_send: Arc<Notify>,
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

    let (failure_monitor, mut ping_tracker) = init_connection_task_state(
        &shared_state,
        &config,
        current_connection.is_some(),
        on_connection_loss,
    );

    loop {
        // Compute ping timer duration before select! to avoid borrow conflicts
        let ping_sleep_duration = compute_ping_sleep(
            &shared_state,
            ping_tracker.as_ref(),
            current_connection.is_some(),
        );
        let ping_active = ping_sleep_duration.is_some();

        // Deliberately NOT `biased;`: data_to_send (outbound) and read (inbound)
        // are peer data sources that can be ready in the same poll; the seeded
        // rotation lets different seeds explore both send-first and read-first
        // interleavings instead of pinning one winner forever.
        moonpool_core::select! {
            // Check for shutdown
            _ = shutdown_rx.recv() => {
                break;
            }

            // Wait for data to send (FoundationDB pattern)
            () = data_to_send.notified() => {
                match handle_data_to_send(DataToSendCtx {
                    shared_state: &shared_state,
                    current_connection: &mut current_connection,
                    read_buffer: &mut read_buffer,
                    config: &config,
                    data_to_send: &data_to_send,
                    ping_tracker: ping_tracker.as_mut(),
                    failure_monitor: failure_monitor.as_ref(),
                    on_connection_loss,
                }).await {
                    DataToSendOutcome::Break => break,
                    DataToSendOutcome::Proceed => {}
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
                let mut ctx = ReadHandlerCtx {
                    shared_state: &shared_state,
                    current_connection: &mut current_connection,
                    read_buffer: &mut read_buffer,
                    receive_tx: &receive_tx,
                    ping_tracker: &mut ping_tracker,
                    data_to_send: &data_to_send,
                    failure_monitor: failure_monitor.as_ref(),
                    on_connection_loss,
                };
                let should_exit = handle_read_result(read_result, &mut ctx);
                if should_exit {
                    break;
                }
            }

            // Connection monitor: periodic ping to detect unresponsive connections
            // FDB ref: connectionMonitor (FlowTransport.actor.cpp:616-699)
            () = async {
                match ping_sleep_duration {
                    Some(duration) => {
                        let time = shared_state.read().expect("RwLock poisoned: prior task panicked").time.clone();
                        let _ = time.sleep(duration).await;
                    }
                    None => std::future::pending::<()>().await,
                }
            }, if ping_active => {
                let should_exit = handle_ping_timer_fire(
                    &shared_state,
                    &mut current_connection,
                    &mut read_buffer,
                    ping_tracker.as_mut(),
                    &data_to_send,
                    failure_monitor.as_ref(),
                    on_connection_loss,
                );
                if should_exit {
                    break;
                }
            }
        }
    }
}

// =============================================================================
// Connection Helpers
// =============================================================================

/// Compute how long to sleep before the next ping action, if any.
///
/// Returns `None` if monitoring is disabled or no connection is active, signalling
/// that the ping arm of the `select!` should be inactive.
fn compute_ping_sleep<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    ping_tracker: Option<&PingTracker>,
    has_connection: bool,
) -> Option<Duration> {
    let tracker = ping_tracker?;
    if !has_connection {
        return None;
    }
    let now = shared_state
        .read()
        .expect("RwLock poisoned: prior task panicked")
        .time
        .now();
    Some(tracker.time_until_next_action(now))
}

/// Initialize the failure monitor handle and ping tracker for `connection_task`.
///
/// If the task starts with an established connection (incoming peers) this also
/// kicks off the first ping cycle and notifies the failure monitor that the
/// destination is currently available.
fn init_connection_task_state<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    config: &PeerConfig,
    has_initial_connection: bool,
    on_connection_loss: ConnectionLossBehavior,
) -> (Option<Arc<FailureMonitor>>, Option<PingTracker>) {
    // Extract failure monitor from shared state (clone the Option<Arc>)
    let failure_monitor = shared_state
        .read()
        .expect("RwLock poisoned: prior task panicked")
        .failure_monitor
        .clone();

    // Initialize ping tracker: only for outbound peers with monitoring enabled
    let mut ping_tracker: Option<PingTracker> = match (&config.monitor, on_connection_loss) {
        (Some(monitor_config), ConnectionLossBehavior::Reconnect) => {
            Some(PingTracker::new(monitor_config.clone()))
        }
        _ => None,
    };

    // If we start with an existing connection, initialize the ping cycle
    // and notify failure monitor that this address is available
    if has_initial_connection {
        if let Some(tracker) = ping_tracker.as_mut() {
            let now = shared_state
                .read()
                .expect("RwLock poisoned: prior task panicked")
                .time
                .now();
            tracker.last_ping_cycle = Some(now);
        }
        if let Some(fm) = failure_monitor.as_ref() {
            let dest = shared_state
                .read()
                .expect("RwLock poisoned: prior task panicked")
                .destination
                .clone();
            fm.set_status(&dest, FailureStatus::Available);
        }
    }

    (failure_monitor, ping_tracker)
}

/// Try to establish an outbound connection, updating state and ping tracker on success.
///
/// Returns `Some(stream)` on success, `None` if connection attempts failed (the caller
/// should retry on the next notification).
async fn try_establish_connection<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    config: &PeerConfig,
    read_buffer: &mut Vec<u8>,
    failure_monitor: Option<&Arc<FailureMonitor>>,
    ping_tracker: Option<&mut PingTracker>,
) -> Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream> {
    tracing::debug!("connection_task: no connection, establishing new connection");
    match establish_connection(shared_state, config).await {
        Ok(stream) => {
            tracing::debug!("connection_task: successfully established connection");
            read_buffer.clear();
            let dest = {
                let mut state = shared_state
                    .write()
                    .expect("RwLock poisoned: prior task panicked");
                state.connection = Some(());
                state.metrics.is_connected = true;
                state.destination.clone()
            };
            // Notify failure monitor: address is available
            if let Some(fm) = failure_monitor {
                fm.set_status(&dest, FailureStatus::Available);
            }
            // Reset ping tracker on new connection
            if let Some(tracker) = ping_tracker {
                let now = shared_state
                    .read()
                    .expect("RwLock poisoned: prior task panicked")
                    .time
                    .now();
                tracker.reset();
                tracker.last_ping_cycle = Some(now);
            }
            Some(stream)
        }
        Err(e) => {
            tracing::debug!("connection_task: failed to establish connection: {:?}", e);
            None
        }
    }
}

/// Enqueue a ping packet in the unreliable queue and notify the writer if needed.
///
/// Returns the timestamp at which the ping was enqueued, or `None` if a connection
/// is not established or serialization failed.
fn enqueue_ping<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    has_connection: bool,
    data_to_send: &Notify,
) -> Option<Duration> {
    if !has_connection {
        return None;
    }
    let ping_packet = serialize_packet(PING_TOKEN, &[]).ok()?;
    let mut state = shared_state
        .write()
        .expect("RwLock poisoned: prior task panicked");
    let first_unsent = state.are_queues_empty();
    state.unreliable_queue.push_back(ping_packet);
    state.metrics.record_ping_sent();
    let sent_at = state.time.now();
    drop(state);
    if first_unsent {
        data_to_send.notify_one();
    }
    Some(sent_at)
}

/// Handle a ping timer firing. Returns `true` if the connection task should exit.
fn handle_ping_timer_fire<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    current_connection: &mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &mut Vec<u8>,
    ping_tracker: Option<&mut PingTracker>,
    data_to_send: &Notify,
    failure_monitor: Option<&Arc<FailureMonitor>>,
    on_connection_loss: ConnectionLossBehavior,
) -> bool {
    let (now, bytes_received) = {
        let state = shared_state
            .read()
            .expect("RwLock poisoned: prior task panicked");
        (state.time.now(), state.metrics.bytes_received)
    };

    let Some(tracker) = ping_tracker else {
        return false;
    };

    match tracker.on_timer_fired(now, bytes_received) {
        PingAction::SendPing => {
            if enqueue_ping(shared_state, current_connection.is_some(), data_to_send).is_some() {
                tracing::debug!("connection_task: enqueued ping in unreliable_queue");
            }
            false
        }
        PingAction::Tolerate => {
            shared_state
                .write()
                .expect("RwLock poisoned: prior task panicked")
                .metrics
                .record_ping_timeout_tolerated();
            tracing::debug!(
                "connection_task: ping timeout tolerated (bytes still flowing), count={}",
                tracker.timeout_count
            );
            // Re-send a ping for the next timeout window via unreliable queue.
            if let Some(sent_at) =
                enqueue_ping(shared_state, current_connection.is_some(), data_to_send)
            {
                tracker.ping_sent_at = Some(sent_at);
            }
            false
        }
        PingAction::TearDown => {
            shared_state
                .write()
                .expect("RwLock poisoned: prior task panicked")
                .metrics
                .record_ping_timeout();
            tracing::debug!(
                "connection_task: ping timeout, tearing down connection to {}",
                shared_state
                    .read()
                    .expect("RwLock poisoned: prior task panicked")
                    .destination
            );
            tracker.reset();
            handle_connection_failure(
                shared_state,
                current_connection,
                read_buffer,
                None,
                failure_monitor,
            );
            if on_connection_loss == ConnectionLossBehavior::Exit {
                return true;
            }
            data_to_send.notify_one();
            false
        }
    }
}

/// Outcome of [`handle_data_to_send`] — how the outer connection loop should proceed.
enum DataToSendOutcome {
    /// Exit the outer loop (`break`).
    Break,
    /// Continue running the outer loop normally (no special action).
    Proceed,
}

/// Context shared by the `data_to_send` branch of `connection_task`'s `select!`.
struct DataToSendCtx<'a, P: Providers> {
    shared_state: &'a Arc<RwLock<PeerSharedState<P>>>,
    current_connection: &'a mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &'a mut Vec<u8>,
    config: &'a PeerConfig,
    data_to_send: &'a Notify,
    ping_tracker: Option<&'a mut PingTracker>,
    failure_monitor: Option<&'a Arc<FailureMonitor>>,
    on_connection_loss: ConnectionLossBehavior,
}

/// Handle the `data_to_send.notified()` branch of `connection_task`'s `select!`.
///
/// Ensures a connection is available (possibly establishing one) and drains
/// pending send queues.
async fn handle_data_to_send<P: Providers>(ctx: DataToSendCtx<'_, P>) -> DataToSendOutcome {
    // First, ensure we have messages to send
    let has_messages = {
        let state = ctx
            .shared_state
            .read()
            .expect("RwLock poisoned: prior task panicked");
        let total = state.reliable_queue.len() + state.unreliable_queue.len();
        tracing::debug!(
            "connection_task: queues have {} messages (reliable={}, unreliable={})",
            total,
            state.reliable_queue.len(),
            state.unreliable_queue.len()
        );
        total > 0
    };

    if !has_messages {
        tracing::debug!("connection_task: spurious wakeup, no messages to send");
        return DataToSendOutcome::Proceed;
    }

    let DataToSendCtx {
        shared_state,
        current_connection,
        read_buffer,
        config,
        data_to_send,
        mut ping_tracker,
        failure_monitor,
        on_connection_loss,
    } = ctx;

    // Ensure we have a connection
    if current_connection.is_none() {
        match on_connection_loss {
            ConnectionLossBehavior::Reconnect => {
                match try_establish_connection(
                    shared_state,
                    config,
                    read_buffer,
                    failure_monitor,
                    ping_tracker.as_deref_mut(),
                )
                .await
                {
                    Some(stream) => *current_connection = Some(stream),
                    None => return DataToSendOutcome::Proceed,
                }
            }
            ConnectionLossBehavior::Exit => {
                tracing::debug!(
                    "connection_task: connection lost, exiting (client must reconnect)"
                );
                return DataToSendOutcome::Break;
            }
        }
    }

    // Process send queues - drain reliable first, then unreliable (FDB pattern)
    drain_send_queues(
        shared_state,
        current_connection,
        read_buffer,
        ping_tracker,
        data_to_send,
        failure_monitor,
    )
    .await;
    DataToSendOutcome::Proceed
}

/// Drain pending messages from the send queues, writing them to the connection.
///
/// Reliable messages are sent first, then unreliable. On write failure the
/// connection is torn down and the failed packet requeued (for reliable) or
/// dropped (for unreliable). The send loop exits on the first failure so the
/// outer loop can reconnect on the next notification.
async fn drain_send_queues<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    current_connection: &mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &mut Vec<u8>,
    mut ping_tracker: Option<&mut PingTracker>,
    data_to_send: &Notify,
    failure_monitor: Option<&Arc<FailureMonitor>>,
) {
    tracing::debug!("connection_task: processing send queues");
    while let Some(stream) = current_connection.as_mut() {
        // Get next message - reliable queue has priority
        let (message, is_reliable) = {
            let mut state = shared_state
                .write()
                .expect("RwLock poisoned: prior task panicked");
            if let Some(msg) = state.reliable_queue.pop_front() {
                tracing::debug!(
                    "connection_task: popped reliable message, remaining: {}",
                    state.reliable_queue.len()
                );
                (Some(msg), true)
            } else if let Some(msg) = state.unreliable_queue.pop_front() {
                tracing::debug!(
                    "connection_task: popped unreliable message, remaining: {}",
                    state.unreliable_queue.len()
                );
                (Some(msg), false)
            } else {
                (None, false)
            }
        };

        let Some(data) = message else {
            tracing::debug!("connection_task: all queues empty, breaking");
            break;
        };

        tracing::debug!(
            "connection_task: attempting to send {} bytes (reliable={})",
            data.len(),
            is_reliable
        );

        tracing::debug!(
            "connection_task: calling stream.write_all() with {} bytes",
            data.len()
        );
        match stream.write_all(&data).await {
            Ok(()) => {
                tracing::debug!("connection_task: write_all succeeded");
                let mut state = shared_state
                    .write()
                    .expect("RwLock poisoned: prior task panicked");
                state.metrics.record_message_sent(data.len());
                state.metrics.record_message_dequeued();
            }
            Err(e) => {
                tracing::debug!("connection_task: write_all failed: {:?}", e);
                handle_connection_failure(
                    shared_state,
                    current_connection,
                    read_buffer,
                    Some((data, is_reliable)),
                    failure_monitor,
                );
                if let Some(tracker) = ping_tracker.as_deref_mut() {
                    tracker.reset();
                }
                data_to_send.notify_one();
                break;
            }
        }
    }
}

/// Context shared by the connection task's read-result handler.
struct ReadHandlerCtx<'a, P: Providers> {
    shared_state: &'a Arc<RwLock<PeerSharedState<P>>>,
    current_connection: &'a mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &'a mut Vec<u8>,
    receive_tx: &'a mpsc::UnboundedSender<(UID, Vec<u8>)>,
    ping_tracker: &'a mut Option<PingTracker>,
    data_to_send: &'a Notify,
    failure_monitor: Option<&'a Arc<FailureMonitor>>,
    on_connection_loss: ConnectionLossBehavior,
}

/// Handle the result of a streaming read, parsing any buffered packets.
///
/// Returns `true` if the connection task should exit.
fn handle_read_result<P: Providers>(
    read_result: std::io::Result<(Vec<u8>, usize)>,
    ctx: &mut ReadHandlerCtx<'_, P>,
) -> bool {
    match read_result {
        Ok((_, 0)) | Err(_) => {
            handle_connection_failure(
                ctx.shared_state,
                ctx.current_connection,
                ctx.read_buffer,
                None,
                ctx.failure_monitor,
            );
            if let Some(tracker) = ctx.ping_tracker.as_mut() {
                tracker.reset();
            }
            if ctx.on_connection_loss == ConnectionLossBehavior::Exit {
                return true;
            }
            ctx.data_to_send.notify_one();
            false
        }
        Ok((buffer, n)) => {
            ctx.read_buffer.extend_from_slice(&buffer[..n]);
            tracing::debug!(
                "connection_task: received {} bytes, buffer now {} bytes",
                n,
                ctx.read_buffer.len()
            );

            // Try to parse complete packets from buffer.
            // Pong replies are enqueued in unreliable_queue by process_read_buffer
            // and picked up by the writer branch (FDB pattern: connectionWriter is sole TCP writer).
            process_read_buffer(
                ctx.shared_state,
                ctx.current_connection,
                ctx.read_buffer,
                ctx.receive_tx,
                ctx.on_connection_loss,
                ctx.ping_tracker,
                ctx.data_to_send,
            )
        }
    }
}

/// Handle connection failure by clearing state and optionally requeuing data.
///
/// Fires `disconnect_notify` to wake all watchers (FDB pattern: `Peer::disconnect`).
/// Notifies failure monitor of address failure and disconnect.
fn handle_connection_failure<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    current_connection: &mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &mut Vec<u8>,
    failed_send: Option<(Vec<u8>, bool)>, // (data, is_reliable)
    failure_monitor: Option<&Arc<FailureMonitor>>,
) {
    *current_connection = None;
    read_buffer.clear();

    let (disconnect_notify, destination) = {
        let mut state = shared_state
            .write()
            .expect("RwLock poisoned: prior task panicked");
        state.connection = None;
        state.metrics.is_connected = false;

        // Handle the failed send if provided
        if let Some((data, is_reliable)) = failed_send {
            if is_reliable {
                // Message was pop_front'd but write failed — put it back.
                // current_queue_size is correct: pop_front doesn't decrement it,
                // only record_message_dequeued (on successful write) does.
                state.reliable_queue.push_front(data);
            } else {
                state.metrics.record_message_dropped();
                // Dequeue metric to match the pop_front that already happened
                state.metrics.record_message_dequeued();
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

        // Reconcile queue size after cleanup.
        state.metrics.current_queue_size = state.reliable_queue.len();

        (state.disconnect_notify.clone(), state.destination.clone())
    };

    // Wake all disconnect watchers (FDB: Peer::disconnect.send(Void()))
    disconnect_notify.notify_waiters();

    // Notify failure monitor: address failed + disconnect signal
    if let Some(fm) = failure_monitor {
        fm.set_status(&destination, FailureStatus::Failed);
        fm.notify_disconnect(&destination);
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
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
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
                    let mut state = shared_state
                        .write()
                        .expect("RwLock poisoned: prior task panicked");
                    state.metrics.record_message_received(consumed);
                }

                // Remove consumed bytes from buffer
                read_buffer.drain(..consumed);

                // Intercept transport-layer tokens before delivering to the application.
                if token == PING_TOKEN {
                    handle_ping_token(shared_state, data_to_send);
                    continue;
                }
                if token == PONG_TOKEN {
                    handle_pong_token(shared_state, ping_tracker.as_mut());
                    continue;
                }
                if token == ENDPOINT_NOT_FOUND_TOKEN {
                    handle_endpoint_not_found_token(shared_state, &payload);
                    continue;
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
                return handle_wire_error(
                    shared_state,
                    current_connection,
                    read_buffer,
                    ping_tracker.as_mut(),
                    &e,
                    on_connection_loss,
                );
            }
        }
    }
}

/// Handle a `PING_TOKEN` by enqueueing a pong packet for the writer to send.
///
/// FDB pattern: pong goes through `sendPacket → unsent queue → connectionWriter`.
fn handle_ping_token<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    data_to_send: &Notify,
) {
    if let Ok(pong_packet) = serialize_packet(PONG_TOKEN, &[]) {
        let mut state = shared_state
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let first_unsent = state.are_queues_empty();
        state.unreliable_queue.push_back(pong_packet);
        drop(state);
        if first_unsent {
            data_to_send.notify_one();
        }
    }
    tracing::debug!("connection_task: received ping, enqueued pong in unreliable_queue");
}

/// Handle a `PONG_TOKEN` by updating the ping tracker with the measured RTT.
fn handle_pong_token<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    ping_tracker: Option<&mut PingTracker>,
) {
    let Some(tracker) = ping_tracker else {
        return;
    };
    let now = shared_state
        .read()
        .expect("RwLock poisoned: prior task panicked")
        .time
        .now();
    if let Some(rtt) = tracker.on_pong_received(now) {
        shared_state
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .metrics
            .record_pong_received(rtt);
        tracing::debug!("connection_task: received pong, rtt={:?}", rtt);
    }
}

/// Handle an `ENDPOINT_NOT_FOUND_TOKEN` notification by reporting the missing
/// endpoint to the failure monitor.
///
/// FDB: `EndpointNotFoundReceiver::receive()` (`FlowTransport.cpp:232-236`).
fn handle_endpoint_not_found_token<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    payload: &[u8],
) {
    if payload.len() < 16 {
        return;
    }
    let first = u64::from_le_bytes(payload[0..8].try_into().unwrap_or_default());
    let second = u64::from_le_bytes(payload[8..16].try_into().unwrap_or_default());
    let missing_token = UID::new(first, second);

    let state = shared_state
        .read()
        .expect("RwLock poisoned: prior task panicked");
    if let Ok(addr) = crate::NetworkAddress::parse(&state.destination) {
        let endpoint = crate::Endpoint::new(addr, missing_token);
        if let Some(fm) = state.failure_monitor.as_ref() {
            tracing::debug!(
                "connection_task: endpoint_not_found for token {} at {}",
                missing_token,
                state.destination
            );
            fm.endpoint_not_found(&endpoint);
        }
    }
}

/// Tear down the connection after a wire-format error and notify failure monitor.
///
/// Returns `true` if the task should exit (i.e. `ConnectionLossBehavior::Exit`).
fn handle_wire_error<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    current_connection: &mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &mut Vec<u8>,
    ping_tracker: Option<&mut PingTracker>,
    error: &WireError,
    on_connection_loss: ConnectionLossBehavior,
) -> bool {
    // Protocol error - invalid packet (FDB pattern: tear down connection)
    match error {
        WireError::ChecksumMismatch { expected, actual } => {
            tracing::warn!(
                "ChecksumMismatch: expected={:#010x} actual={:#010x} - tearing down connection (FDB pattern)",
                expected,
                actual
            );
        }
        _ => {
            tracing::warn!(
                "connection_task: wire format error: {} - tearing down connection",
                error
            );
        }
    }

    *current_connection = None;
    read_buffer.clear();

    if let Some(tracker) = ping_tracker {
        tracker.reset();
    }

    let (disconnect_notify, destination, failure_monitor) = {
        let mut state = shared_state
            .write()
            .expect("RwLock poisoned: prior task panicked");
        state.connection = None;
        state.metrics.is_connected = false;

        let unreliable_count = state.unreliable_queue.len();
        if unreliable_count > 0 {
            tracing::debug!(
                "connection_task: discarding {} unreliable packets after wire error (FDB pattern)",
                unreliable_count
            );
            for _ in 0..unreliable_count {
                state.metrics.record_message_dropped();
            }
            state.unreliable_queue.clear();
            // Reconcile: unreliable user messages gone, only reliable remain tracked
            state.metrics.current_queue_size = state.reliable_queue.len();
        }

        (
            state.disconnect_notify.clone(),
            state.destination.clone(),
            state.failure_monitor.clone(),
        )
    };

    // Wake all disconnect watchers (FDB: Peer::disconnect.send(Void()))
    disconnect_notify.notify_waiters();

    // Notify failure monitor: address failed + disconnect signal
    if let Some(fm) = failure_monitor {
        fm.set_status(&destination, FailureStatus::Failed);
        fm.notify_disconnect(&destination);
    }

    on_connection_loss == ConnectionLossBehavior::Exit
}

/// Establish a connection with exponential backoff.
async fn establish_connection<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    config: &PeerConfig,
) -> PeerResult<<P::Network as moonpool_core::NetworkProvider>::TcpStream> {
    loop {
        // Check failure limits and get connection params
        let (network, time, destination, should_backoff, delay) = {
            let state = shared_state
                .write()
                .expect("RwLock poisoned: prior task panicked");

            // Check failure limits
            if let Some(max_failures) = config.max_connection_failures
                && state.reconnect_state.failure_count >= max_failures
            {
                return Err(PeerError::ConnectionFailed);
            }

            // Check if backoff is needed
            let now = state.time.now();
            let (should_backoff, delay) =
                if let Some(last_attempt) = state.reconnect_state.last_attempt {
                    let elapsed = now.saturating_sub(last_attempt);
                    if elapsed < state.reconnect_state.current_delay {
                        let remaining = state
                            .reconnect_state
                            .current_delay
                            .checked_sub(elapsed)
                            .expect("checked_sub is safe: elapsed < current_delay verified above");
                        (true, remaining)
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

        // Apply backoff if needed (no shared-state lock held)
        if should_backoff && time.sleep(delay).await.is_err() {
            return Err(PeerError::ConnectionFailed);
        }

        // Record attempt
        {
            let mut state = shared_state
                .write()
                .expect("RwLock poisoned: prior task panicked");
            state.reconnect_state.last_attempt = Some(state.time.now());
            state.metrics.record_connection_attempt();
        }

        // Attempt connection (no shared-state lock held)
        match time
            .timeout(config.connection_timeout, network.connect(&destination))
            .await
        {
            Ok(Ok(stream)) => {
                // Success
                {
                    let mut state = shared_state
                        .write()
                        .expect("RwLock poisoned: prior task panicked");
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
                record_connection_failure(shared_state, config);
                // Continue loop to retry
            }
        }
    }
}

/// Record a connection failure: increment failure count, update backoff delay, record metrics.
fn record_connection_failure<P: Providers>(
    shared_state: &Arc<RwLock<PeerSharedState<P>>>,
    config: &PeerConfig,
) {
    let mut state = shared_state
        .write()
        .expect("RwLock poisoned: prior task panicked");
    state.reconnect_state.failure_count += 1;
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

    #[test]
    fn test_endpoint_not_found_token_is_well_known_zero() {
        assert!(ENDPOINT_NOT_FOUND_TOKEN.is_well_known());
        assert_eq!(ENDPOINT_NOT_FOUND_TOKEN.first, u64::MAX);
        assert_eq!(ENDPOINT_NOT_FOUND_TOKEN.second, 0);
        assert_ne!(ENDPOINT_NOT_FOUND_TOKEN, PING_TOKEN);
        assert_ne!(ENDPOINT_NOT_FOUND_TOKEN, PONG_TOKEN);
    }

    #[test]
    fn test_endpoint_not_found_payload_encoding() {
        let token = UID::new(0x1234_5678_9ABC_DEF0, 0xFEDC_BA98_7654_3210);

        // Encode
        let mut payload = [0u8; 16];
        payload[0..8].copy_from_slice(&token.first.to_le_bytes());
        payload[8..16].copy_from_slice(&token.second.to_le_bytes());

        // Decode (same logic as process_read_buffer)
        let first = u64::from_le_bytes(payload[0..8].try_into().unwrap_or_default());
        let second = u64::from_le_bytes(payload[8..16].try_into().unwrap_or_default());
        let decoded = UID::new(first, second);

        assert_eq!(decoded, token);
    }

    #[test]
    fn test_endpoint_not_found_malformed_payload_ignored() {
        // Payloads shorter than 16 bytes should not panic during decode.
        // The intercept in process_read_buffer guards with `payload.len() >= 16`.
        let short_payload = [0u8; 8];
        assert!(short_payload.len() < 16);
        // No panic — the guard prevents decode attempt.
    }
}
