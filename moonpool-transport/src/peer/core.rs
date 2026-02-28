//! Core peer implementation with automatic reconnection and message queuing.
//!
//! Provides wire format with UID-based endpoint addressing.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use super::config::PeerConfig;
use super::error::{PeerError, PeerResult};
use super::metrics::PeerMetrics;
use crate::{
    HEADER_SIZE, NetworkProvider, Providers, TaskProvider, TimeProvider, UID, WellKnownToken,
    WireError, serialize_packet, try_deserialize_packet,
};
use moonpool_sim::{assert_sometimes, assert_sometimes_each};

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

    /// Shutdown signaling for connection_task
    shutdown_tx: mpsc::UnboundedSender<()>,

    /// Shutdown signaling for connection_monitor
    monitor_shutdown_tx: Option<mpsc::UnboundedSender<()>>,

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

    /// Notify signaled when a pong arrives.
    ///
    /// Replaced before each ping cycle to avoid spurious notifications.
    /// FDB Reference: connectionMonitor uses choose{} to race pong vs timeout.
    pong_notify: Rc<Notify>,

    /// Time when the last ping was sent (for RTT calculation).
    last_ping_sent_at: Option<Duration>,
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
            pong_notify: Rc::new(Notify::new()),
            last_ping_sent_at: None,
        }));

        // Create coordination primitives
        let data_to_send = Rc::new(Notify::new());
        let (receive_tx, receive_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();
        let ping_timeout_notify = Rc::new(Notify::new());

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
                ping_timeout_notify.clone(),
            ),
        );

        // Spawn connection monitor if ping is enabled
        let monitor_shutdown_tx = if config.ping_interval > Duration::ZERO {
            let (monitor_tx, monitor_rx) = mpsc::unbounded_channel();
            providers.task().spawn_task(
                "connection_monitor",
                connection_monitor(
                    shared_state.clone(),
                    data_to_send.clone(),
                    config.clone(),
                    ping_timeout_notify,
                    monitor_rx,
                ),
            );
            Some(monitor_tx)
        } else {
            None
        };

        Self {
            shared_state,
            data_to_send,
            writer_handle: Some(writer_handle),
            receive_rx: Some(receive_rx),
            shutdown_tx,
            monitor_shutdown_tx,
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
            pong_notify: Rc::new(Notify::new()),
            last_ping_sent_at: None,
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
        let ping_timeout_notify = Rc::new(Notify::new());

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
                ping_timeout_notify.clone(),
            ),
        );

        // Spawn connection monitor if ping is enabled
        let monitor_shutdown_tx = if config.ping_interval > Duration::ZERO {
            let (monitor_tx, monitor_rx) = mpsc::unbounded_channel();
            providers.task().spawn_task(
                "connection_monitor",
                connection_monitor(
                    shared_state.clone(),
                    data_to_send.clone(),
                    config.clone(),
                    ping_timeout_notify,
                    monitor_rx,
                ),
            );
            Some(monitor_tx)
        } else {
            None
        };

        Self {
            shared_state,
            data_to_send,
            writer_handle: Some(writer_handle),
            receive_rx: Some(receive_rx),
            shutdown_tx,
            monitor_shutdown_tx,
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

        // Signal shutdown to connection monitor
        if let Some(ref tx) = self.monitor_shutdown_tx {
            let _ = tx.send(());
        }

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
/// Matches FoundationDB's connectionWriter pattern:
/// - Waits for dataToSend trigger
/// - Drains unsent queue continuously
/// - Handles connection failures and reconnection (or exit for incoming)
/// - Owns the connection exclusively to avoid RefCell conflicts
/// - Handles both reading and writing operations
/// - Parses wire format packets from the read stream
#[allow(clippy::too_many_arguments)]
async fn connection_task<P: Providers>(
    shared_state: Rc<RefCell<PeerSharedState<P>>>,
    data_to_send: Rc<Notify>,
    config: PeerConfig,
    receive_tx: mpsc::UnboundedSender<(UID, Vec<u8>)>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
    initial_stream: Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    on_connection_loss: ConnectionLossBehavior,
    ping_timeout_notify: Rc<Notify>,
) {
    let mut current_connection: Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream> =
        initial_stream;
    // Buffer for accumulating partial packet reads
    let mut read_buffer: Vec<u8> = Vec::with_capacity(4096);
    let ping_enabled = config.ping_interval > Duration::ZERO;

    loop {
        tokio::select! {
            // Check for shutdown
            _ = shutdown_rx.recv() => {
                break;
            }

            // Check for ping timeout from connection_monitor
            _ = ping_timeout_notify.notified(), if ping_enabled && current_connection.is_some() => {
                tracing::warn!("connection_task: ping timeout detected by connection_monitor");
                assert_sometimes!(true, "Ping timeout triggers connection failure");
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
                        if on_connection_loss == ConnectionLossBehavior::Exit {
                            break;
                        }
                    }
                    Ok((buffer, n)) => {
                        // Append to read buffer
                        read_buffer.extend_from_slice(&buffer[..n]);
                        tracing::debug!("connection_task: received {} bytes, buffer now {} bytes", n, read_buffer.len());

                        // Try to parse complete packets from buffer
                        let should_exit = process_read_buffer(
                            &shared_state,
                            &mut current_connection,
                            &mut read_buffer,
                            &receive_tx,
                            on_connection_loss,
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
                        if on_connection_loss == ConnectionLossBehavior::Exit {
                            break;
                        }
                    }
                }
            }
        }
    }
}

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
/// Returns true if the task should exit.
fn process_read_buffer<P: Providers>(
    shared_state: &Rc<RefCell<PeerSharedState<P>>>,
    current_connection: &mut Option<<P::Network as moonpool_core::NetworkProvider>::TcpStream>,
    read_buffer: &mut Vec<u8>,
    receive_tx: &mpsc::UnboundedSender<(UID, Vec<u8>)>,
    on_connection_loss: ConnectionLossBehavior,
    data_to_send: &Rc<Notify>,
) -> bool {
    let ping_token = WellKnownToken::Ping.uid();

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

                // Intercept ping/pong packets (FDB: PingReceiver pattern)
                if token == ping_token && !payload.is_empty() {
                    handle_ping_pong_packet(shared_state, &payload, data_to_send);
                    continue; // Don't forward to receive_tx
                }

                // Send to receiver
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
// Ping/Pong Protocol
// =============================================================================
//
// FDB Reference: connectionMonitor (FlowTransport.actor.cpp:616-699)
//                PingReceiver (FlowTransport.actor.cpp:248-261)
//
// Wire format (uses WellKnownToken::Ping):
//   Ping: [0u8][timestamp_nanos: u64 LE]  = 9 bytes
//   Pong: [1u8][echoed_timestamp_nanos: u64 LE] = 9 bytes
//
// The timestamp is the sender's simulation time when the ping was sent,
// echoed back in the pong for RTT calculation.

/// Ping type marker byte.
const PING_TYPE_REQUEST: u8 = 0;
/// Pong type marker byte.
const PONG_TYPE_RESPONSE: u8 = 1;
/// Expected size of a ping/pong payload.
const PING_PONG_PAYLOAD_SIZE: usize = 9;

/// Create a ping payload with the given timestamp.
fn create_ping_payload(now: Duration) -> Vec<u8> {
    let mut payload = Vec::with_capacity(PING_PONG_PAYLOAD_SIZE);
    payload.push(PING_TYPE_REQUEST);
    let timestamp_nanos = now.as_nanos() as u64;
    payload.extend_from_slice(&timestamp_nanos.to_le_bytes());
    payload
}

/// Create a pong payload echoing the timestamp from a ping.
fn create_pong_payload(ping_payload: &[u8]) -> Option<Vec<u8>> {
    if ping_payload.len() < PING_PONG_PAYLOAD_SIZE {
        return None;
    }
    let mut payload = Vec::with_capacity(PING_PONG_PAYLOAD_SIZE);
    payload.push(PONG_TYPE_RESPONSE);
    // Echo the timestamp bytes from the ping
    payload.extend_from_slice(&ping_payload[1..PING_PONG_PAYLOAD_SIZE]);
    Some(payload)
}

/// Parse the timestamp from a pong payload.
fn parse_pong_timestamp(payload: &[u8]) -> Option<Duration> {
    if payload.len() < PING_PONG_PAYLOAD_SIZE || payload[0] != PONG_TYPE_RESPONSE {
        return None;
    }
    let nanos = u64::from_le_bytes(payload[1..PING_PONG_PAYLOAD_SIZE].try_into().ok()?);
    Some(Duration::from_nanos(nanos))
}

/// Handle an intercepted ping or pong packet.
///
/// - Ping (type 0): Queue a pong response in the unreliable queue.
/// - Pong (type 1): Calculate RTT, update metrics, signal pong_notify.
///
/// FDB Reference: PingReceiver::receive() (FlowTransport.actor.cpp:253-256)
fn handle_ping_pong_packet<P: Providers>(
    shared_state: &Rc<RefCell<PeerSharedState<P>>>,
    payload: &[u8],
    data_to_send: &Rc<Notify>,
) {
    let ping_token = WellKnownToken::Ping.uid();

    match payload[0] {
        PING_TYPE_REQUEST => {
            // Received a ping - respond with pong (FDB: req.reply.send(Void()))
            tracing::debug!("handle_ping_pong: received ping, sending pong");
            if let Some(pong_payload) = create_pong_payload(payload)
                && let Ok(packet) = serialize_packet(ping_token, &pong_payload)
            {
                let mut state = shared_state.borrow_mut();
                let was_empty = state.are_queues_empty();
                state.unreliable_queue.push_back(packet);
                drop(state);
                if was_empty {
                    data_to_send.notify_one();
                }
            }
        }
        PONG_TYPE_RESPONSE => {
            // Received a pong - calculate RTT and signal monitor
            tracing::debug!("handle_ping_pong: received pong");
            if let Some(sent_at) = parse_pong_timestamp(payload) {
                let mut state = shared_state.borrow_mut();
                let now = state.time.now();
                let rtt = now.saturating_sub(sent_at);
                state.metrics.record_pong_received(now, rtt);
                state.last_ping_sent_at = None;
                let notify = state.pong_notify.clone();
                drop(state);
                notify.notify_one();
                assert_sometimes!(true, "Pong received and RTT recorded");
            }
        }
        _ => {
            tracing::warn!("handle_ping_pong: unknown ping/pong type: {}", payload[0]);
        }
    }
}

/// Background connection health monitor.
///
/// Periodically sends ping packets and waits for pong responses.
/// If no pong arrives within `ping_timeout`, signals the connection_task
/// to drop the connection and reconnect.
///
/// FDB Reference: connectionMonitor (FlowTransport.actor.cpp:616-699)
///
/// # Design
///
/// - Sends pings via the unreliable queue (like FDB's sendUnreliable)
/// - Uses a fresh Notify per ping cycle to avoid spurious wakeups
/// - The timeout notification is picked up by connection_task's select! loop
/// - Only pings when the connection is established
async fn connection_monitor<P: Providers>(
    shared_state: Rc<RefCell<PeerSharedState<P>>>,
    data_to_send: Rc<Notify>,
    config: PeerConfig,
    ping_timeout_notify: Rc<Notify>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
) {
    let ping_token = WellKnownToken::Ping.uid();

    loop {
        // Sleep for ping interval before sending
        {
            let time = shared_state.borrow().time.clone();
            let result = time.timeout(config.ping_interval, shutdown_rx.recv());
            match result.await {
                Ok(_) => {
                    // Shutdown signal received
                    tracing::debug!("connection_monitor: shutdown during ping interval");
                    return;
                }
                Err(_) => {
                    // Timeout elapsed - time to ping
                }
            }
        }

        // Create a fresh Notify for this ping cycle to avoid spurious wakeups
        let pong_notify = Rc::new(Notify::new());
        let now = {
            let mut state = shared_state.borrow_mut();
            state.pong_notify = pong_notify.clone();
            state.time.now()
        };

        // Create and queue ping packet.
        // Always notify data_to_send so connection_task will establish a
        // connection (if needed) and flush the queued ping.
        let ping_payload = create_ping_payload(now);
        if let Ok(packet) = serialize_packet(ping_token, &ping_payload) {
            {
                let mut state = shared_state.borrow_mut();
                state.last_ping_sent_at = Some(now);
                state.metrics.record_ping_sent();
                state.unreliable_queue.push_back(packet);
            }
            data_to_send.notify_one();
        }

        // Wait for pong or timeout
        // FDB: choose { when(delay(TIMEOUT)) {...} when(reply.getFuture()) {...} }
        let time = shared_state.borrow().time.clone();
        match time
            .timeout(config.ping_timeout, pong_notify.notified())
            .await
        {
            Ok(()) => {
                // Pong received within timeout - connection healthy
                tracing::debug!("connection_monitor: pong received, connection healthy");
            }
            Err(_) => {
                // Timeout - no pong received
                // FDB: throw connection_failed() (FlowTransport.actor.cpp:672-678)

                // Double-check: connection might have been dropped by another path
                let is_connected = shared_state.borrow().connection.is_some();
                if is_connected {
                    tracing::warn!(
                        "connection_monitor: ping timeout - signaling connection failure"
                    );
                    shared_state.borrow_mut().metrics.record_ping_timeout();
                    ping_timeout_notify.notify_one();
                } else {
                    tracing::debug!(
                        "connection_monitor: ping timeout but connection already dropped"
                    );
                }
            }
        }
    }
}
