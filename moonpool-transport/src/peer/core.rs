//! Core peer implementation with automatic reconnection and message queuing.
//!
//! Provides FDB-compatible wire format with UID-based endpoint addressing.

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
    HEADER_SIZE, NetworkProvider, TaskProvider, TimeProvider, UID, WireError, serialize_packet,
    try_deserialize_packet,
};
use moonpool_sim::sometimes_assert;

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
/// over NetworkProvider, TimeProvider, and TaskProvider implementations.
///
/// Uses FDB-compatible wire format: `[length:4][checksum:4][token:16][payload]`
///
/// Follows FoundationDB's architecture: synchronous API with background actors.
pub struct Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    /// Shared state accessible to background actors
    shared_state: Rc<RefCell<PeerSharedState<N, T>>>,

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

    /// Task provider for spawning background actors
    #[allow(dead_code)]
    task_provider: TP,
}

/// Shared state for background actors - each actor accesses different fields
struct PeerSharedState<N: NetworkProvider, T: TimeProvider> {
    /// Network provider for creating connections
    network: N,

    /// Time provider for delays and timing
    time: T,

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

impl<N: NetworkProvider + 'static, T: TimeProvider + 'static, TP: TaskProvider + 'static>
    Peer<N, T, TP>
{
    /// Create a new peer for the destination address.
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
        destination: String,
        config: PeerConfig,
    ) -> Self {
        let reconnect_state = ReconnectState::new(config.initial_reconnect_delay);
        let now = time.now();

        // Create shared state
        let shared_state = Rc::new(RefCell::new(PeerSharedState {
            network,
            time,
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
        let writer_handle = task_provider.spawn_task(
            "connection_task",
            connection_task(
                shared_state.clone(),
                data_to_send.clone(),
                config.clone(),
                receive_tx,
                shutdown_rx,
                task_provider.clone(),
            ),
        );

        Self {
            shared_state,
            data_to_send,
            writer_handle: Some(writer_handle),
            receive_rx: Some(receive_rx),
            shutdown_tx,
            config,
            task_provider,
        }
    }

    /// Create a new peer with default configuration.
    pub fn new_with_defaults(network: N, time: T, task_provider: TP, destination: String) -> Self {
        Self::new(
            network,
            time,
            task_provider,
            destination,
            PeerConfig::default(),
        )
    }

    /// Create a new peer from an incoming (already-connected) stream.
    ///
    /// FDB Pattern: `Peer::onIncomingConnection()` (FlowTransport.actor.cpp:1123)
    ///
    /// Used by server-side listener to wrap accepted connections.
    /// Unlike `new()`, this starts with an established connection instead of
    /// initiating an outbound connection.
    pub fn new_incoming(
        network: N,
        time: T,
        task_provider: TP,
        peer_address: String,
        stream: N::TcpStream,
        config: PeerConfig,
    ) -> Self {
        let reconnect_state = ReconnectState::new(config.initial_reconnect_delay);
        let now = time.now();

        // Create shared state - mark as connected since we have an existing stream
        let shared_state = Rc::new(RefCell::new(PeerSharedState {
            network,
            time,
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
        let writer_handle = task_provider.spawn_task(
            "incoming_connection_task",
            incoming_connection_task(
                shared_state.clone(),
                data_to_send.clone(),
                config.clone(),
                receive_tx,
                shutdown_rx,
                stream,
            ),
        );

        Self {
            shared_state,
            data_to_send,
            writer_handle: Some(writer_handle),
            receive_rx: Some(receive_rx),
            shutdown_tx,
            config,
            task_provider,
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
        // Serialize packet with wire format
        let packet = serialize_packet(token, payload).map_err(|e| match e {
            WireError::PacketTooLarge { size } => {
                PeerError::InvalidOperation(format!("payload too large: {} bytes", size))
            }
            _ => PeerError::InvalidOperation(format!("serialization error: {}", e)),
        })?;

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
            sometimes_assert!(
                peer_queue_near_capacity,
                state.reliable_queue.len() >= (self.config.max_queue_size as f64 * 0.8) as usize,
                "Message queue should sometimes approach capacity limit"
            );

            // Handle queue overflow
            if state.reliable_queue.len() >= self.config.max_queue_size
                && state.reliable_queue.pop_front().is_some()
            {
                state.metrics.record_message_dropped();
            }

            let first_unsent = state.reliable_queue.is_empty() && state.unreliable_queue.is_empty();
            state.reliable_queue.push_back(packet);
            state.metrics.record_message_queued();
            tracing::debug!(
                "Peer::send_reliable queued packet, reliable_queue size now: {}, first_unsent: {}",
                state.reliable_queue.len(),
                first_unsent
            );

            // Check if queue is growing with multiple messages
            sometimes_assert!(
                peer_queue_grows,
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
        // Serialize packet with wire format
        let packet = serialize_packet(token, payload).map_err(|e| match e {
            WireError::PacketTooLarge { size } => {
                PeerError::InvalidOperation(format!("payload too large: {} bytes", size))
            }
            _ => PeerError::InvalidOperation(format!("serialization error: {}", e)),
        })?;

        tracing::debug!(
            "Peer::send_unreliable called with token={}, payload={} bytes",
            token,
            payload.len()
        );

        // Queue packet in unreliable queue (will be discarded on failure)
        {
            let mut state = self.shared_state.borrow_mut();
            let first_unsent = state.reliable_queue.is_empty() && state.unreliable_queue.is_empty();
            state.unreliable_queue.push_back(packet);
            state.metrics.record_message_queued();

            if first_unsent {
                self.data_to_send.notify_one();
            }
        }

        Ok(())
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

/// Background connection task that handles all async TCP I/O.
///
/// Matches FoundationDB's connectionWriter pattern:
/// - Waits for dataToSend trigger
/// - Drains unsent queue continuously
/// - Handles connection failures and reconnection
/// - Owns the connection exclusively to avoid RefCell conflicts
/// - Handles both reading and writing operations
/// - Parses wire format packets from the read stream
async fn connection_task<
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
>(
    shared_state: Rc<RefCell<PeerSharedState<N, T>>>,
    data_to_send: Rc<Notify>,
    config: PeerConfig,
    receive_tx: mpsc::UnboundedSender<(UID, Vec<u8>)>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
    _task_provider: TP,
) {
    let mut current_connection: Option<N::TcpStream> = None;
    // Buffer for accumulating partial packet reads
    let mut read_buffer: Vec<u8> = Vec::with_capacity(4096);

    loop {
        tokio::select! {
            // Check for shutdown
            _ = shutdown_rx.recv() => {
                break;
            }

            // Wait for data to send (FoundationDB pattern)
            _ = data_to_send.notified() => {
                tracing::debug!("connection_task: notified to wake up, checking for messages");
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

                tracing::debug!("connection_task: has messages to send, proceeding");

                // Ensure we have a connection
                if current_connection.is_none() {
                    tracing::debug!("connection_task: no connection, establishing new connection");
                    match establish_connection(&shared_state, &config).await {
                        Ok(stream) => {
                            tracing::debug!("connection_task: successfully established connection");
                            current_connection = Some(stream);
                            read_buffer.clear(); // Clear buffer on new connection
                            {
                                let mut state = shared_state.borrow_mut();
                                state.connection = Some(()); // Mark as connected
                                state.metrics.is_connected = true;
                            }
                        }
                        Err(e) => {
                            tracing::debug!("connection_task: failed to establish connection: {:?}", e);
                            continue; // Will retry on next notification
                        }
                    }
                } else {
                    tracing::debug!("connection_task: using existing connection");
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
                        // Simulate write failure - handle like real failure
                        current_connection = None;
                        read_buffer.clear();
                        {
                            let mut state = shared_state.borrow_mut();
                            state.connection = None;
                            state.metrics.is_connected = false;

                            // Only requeue if reliable (FDB pattern: discardUnreliablePackets)
                            if is_reliable {
                                sometimes_assert!(
                                    peer_requeues_on_failure,
                                    true,
                                    "Peer should sometimes re-queue reliable messages after send failure"
                                );
                                state.reliable_queue.push_front(data);
                            } else {
                                // Unreliable packet is dropped
                                state.metrics.record_message_dropped();
                            }

                            // Discard all remaining unreliable packets (FDB pattern)
                            let unreliable_count = state.unreliable_queue.len();
                            if unreliable_count > 0 {
                                tracing::debug!("connection_task: discarding {} unreliable packets on failure",
                                    unreliable_count);
                                for _ in 0..unreliable_count {
                                    state.metrics.record_message_dropped();
                                }
                                state.unreliable_queue.clear();
                            }
                        }
                        break; // Exit send loop, will retry on next trigger
                    }

                    // Send the message (no RefCell borrow held)
                    tracing::debug!("connection_task: calling stream.write_all() with {} bytes", data.len());
                    match stream.write_all(&data).await {
                        Ok(_) => {
                            // Success
                            tracing::debug!("connection_task: write_all succeeded");
                            {
                                let mut state = shared_state.borrow_mut();
                                state.metrics.record_message_sent(data.len());
                                state.metrics.record_message_dequeued();
                            }
                        }
                        Err(e) => {
                            tracing::debug!("connection_task: write_all failed: {:?}", e);
                            // Write failed - connection is bad
                            current_connection = None;
                            read_buffer.clear();
                            {
                                let mut state = shared_state.borrow_mut();
                                state.connection = None;
                                state.metrics.is_connected = false;

                                // Only requeue if reliable (FDB pattern: discardUnreliablePackets)
                                if is_reliable {
                                    sometimes_assert!(
                                        peer_requeues_on_failure,
                                        true,
                                        "Peer should sometimes re-queue reliable messages after send failure"
                                    );
                                    state.reliable_queue.push_front(data);
                                } else {
                                    // Unreliable packet is dropped
                                    state.metrics.record_message_dropped();
                                }

                                // Discard all remaining unreliable packets (FDB pattern)
                                let unreliable_count = state.unreliable_queue.len();
                                if unreliable_count > 0 {
                                    tracing::debug!("connection_task: discarding {} unreliable packets on failure",
                                        unreliable_count);
                                    for _ in 0..unreliable_count {
                                        state.metrics.record_message_dropped();
                                    }
                                    state.unreliable_queue.clear();
                                }
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
                        current_connection = None;
                        read_buffer.clear();
                        {
                            let mut state = shared_state.borrow_mut();
                            state.connection = None;
                            state.metrics.is_connected = false;
                        }
                    }
                    Ok((buffer, n)) => {
                        // Append to read buffer
                        read_buffer.extend_from_slice(&buffer[..n]);
                        tracing::debug!("connection_task: received {} bytes, buffer now {} bytes", n, read_buffer.len());

                        // Try to parse complete packets from buffer
                        loop {
                            if read_buffer.len() < HEADER_SIZE {
                                break; // Need more data for header
                            }

                            match try_deserialize_packet(&read_buffer) {
                                Ok(Some((token, payload, consumed))) => {
                                    // Successfully parsed a packet
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

                                    // Send to receiver
                                    if receive_tx.send((token, payload)).is_err() {
                                        return; // Receiver dropped
                                    }
                                }
                                Ok(None) => {
                                    // Need more data
                                    break;
                                }
                                Err(e) => {
                                    // Protocol error - invalid packet
                                    // FDB pattern: Tear down connection on wire errors
                                    // (FlowTransport.actor.cpp:889-960, 1325)
                                    use crate::WireError;
                                    match &e {
                                        WireError::ChecksumMismatch { expected, actual } => {
                                            // Checksum validation caught corruption
                                            // This could be:
                                            // 1. Expected: intentional chaos injection (check for BitFlipInjected log)
                                            // 2. Unexpected: real bug in network/serialization code (BUG!)
                                            tracing::warn!(
                                                "ChecksumMismatch: expected={:#010x} actual={:#010x} - tearing down connection (FDB pattern)",
                                                expected,
                                                actual
                                            );
                                            sometimes_assert!(
                                                checksum_caught_corruption,
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

                                    // FDB pattern: Full connection teardown on wire errors
                                    // Matches FlowTransport.actor.cpp:889-960 error handling
                                    sometimes_assert!(
                                        connection_teardown_on_wire_error,
                                        true,
                                        "Connection should sometimes be torn down on wire format errors"
                                    );
                                    current_connection = None;
                                    read_buffer.clear();

                                    {
                                        let mut state = shared_state.borrow_mut();
                                        state.connection = None;
                                        state.metrics.is_connected = false;

                                        // FDB's discardUnreliablePackets() pattern (line 923, 1072-1082)
                                        // Drop all unreliable messages, keep reliable for retry
                                        let unreliable_count = state.unreliable_queue.len();
                                        if unreliable_count > 0 {
                                            tracing::debug!(
                                                "connection_task: discarding {} unreliable packets after wire error (FDB pattern)",
                                                unreliable_count
                                            );
                                            sometimes_assert!(
                                                unreliable_discarded_on_error,
                                                true,
                                                "Unreliable packets should sometimes be discarded on connection errors"
                                            );
                                            for _ in 0..unreliable_count {
                                                state.metrics.record_message_dropped();
                                            }
                                            state.unreliable_queue.clear();
                                        }
                                        // Note: reliable_queue remains intact for automatic retry
                                    }

                                    // Break exits packet parse loop -> select! continues -> will reconnect
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Read error - connection likely broken
                        current_connection = None;
                        read_buffer.clear();
                        {
                            let mut state = shared_state.borrow_mut();
                            state.connection = None;
                            state.metrics.is_connected = false;
                        }
                    }
                }
            }
        }
    }
}

/// Establish a connection with exponential backoff.
async fn establish_connection<N: NetworkProvider + 'static, T: TimeProvider + 'static>(
    shared_state: &Rc<RefCell<PeerSharedState<N, T>>>,
    config: &PeerConfig,
) -> PeerResult<N::TcpStream> {
    loop {
        // Check failure limits and get connection params
        let (network, time, destination, should_backoff, delay) = {
            let state = shared_state.borrow_mut();

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
            Ok(Ok(Ok(stream))) => {
                // Success - check if this was a recovery after failures
                {
                    let mut state = shared_state.borrow_mut();

                    if state.reconnect_state.failure_count > 0 {
                        sometimes_assert!(
                            peer_recovers_after_failures,
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
            Ok(Ok(Err(_))) | Ok(Err(())) | Err(_) => {
                // Connection failed - update state and retry
                {
                    let mut state = shared_state.borrow_mut();
                    state.reconnect_state.failure_count += 1;
                    let next_delay = std::cmp::min(
                        state.reconnect_state.current_delay * 2,
                        config.max_reconnect_delay,
                    );
                    state.reconnect_state.current_delay = next_delay;
                    let now = state.time.now();
                    state.metrics.record_connection_failure_at(now, next_delay);
                }
                // Continue loop to retry
            }
        }
    }
}

/// Background connection task for incoming (already-connected) streams.
///
/// FDB Pattern: `Peer::onIncomingConnection()` (FlowTransport.actor.cpp:1123)
///
/// Unlike `connection_task`, this starts with an existing stream and does NOT
/// attempt reconnection if the connection is lost (server-side connections
/// are not re-established - the client must reconnect).
async fn incoming_connection_task<N: NetworkProvider + 'static, T: TimeProvider + 'static>(
    shared_state: Rc<RefCell<PeerSharedState<N, T>>>,
    data_to_send: Rc<Notify>,
    _config: PeerConfig,
    receive_tx: mpsc::UnboundedSender<(UID, Vec<u8>)>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
    initial_stream: N::TcpStream,
) {
    let mut current_connection: Option<N::TcpStream> = Some(initial_stream);
    let mut read_buffer: Vec<u8> = Vec::with_capacity(4096);

    loop {
        tokio::select! {
            // Check for shutdown
            _ = shutdown_rx.recv() => {
                break;
            }

            // Wait for data to send
            _ = data_to_send.notified() => {
                let has_messages = {
                    let state = shared_state.borrow();
                    state.reliable_queue.len() + state.unreliable_queue.len() > 0
                };

                if !has_messages {
                    continue;
                }

                // For incoming connections, if disconnected we just stop
                // (no reconnection - client must reconnect)
                if current_connection.is_none() {
                    tracing::debug!("incoming_connection_task: connection lost, cannot send (client must reconnect)");
                    break;
                }

                // Process send queues
                while let Some(ref mut stream) = current_connection {
                    let (message, is_reliable) = {
                        let mut state = shared_state.borrow_mut();
                        if let Some(msg) = state.reliable_queue.pop_front() {
                            (Some(msg), true)
                        } else if let Some(msg) = state.unreliable_queue.pop_front() {
                            (Some(msg), false)
                        } else {
                            (None, false)
                        }
                    };

                    let Some(data) = message else {
                        break;
                    };

                    // Buggify: Sometimes force write failures
                    if moonpool_sim::buggify_with_prob!(0.02) {
                        tracing::debug!("Buggify forcing write failure on incoming connection");
                        current_connection = None;
                        read_buffer.clear();
                        {
                            let mut state = shared_state.borrow_mut();
                            state.connection = None;
                            state.metrics.is_connected = false;

                            if is_reliable {
                                state.reliable_queue.push_front(data);
                            } else {
                                state.metrics.record_message_dropped();
                            }

                            let unreliable_count = state.unreliable_queue.len();
                            for _ in 0..unreliable_count {
                                state.metrics.record_message_dropped();
                            }
                            state.unreliable_queue.clear();
                        }
                        break;
                    }

                    match stream.write_all(&data).await {
                        Ok(_) => {
                            let mut state = shared_state.borrow_mut();
                            state.metrics.record_message_sent(data.len());
                            state.metrics.record_message_dequeued();
                        }
                        Err(_) => {
                            current_connection = None;
                            read_buffer.clear();
                            {
                                let mut state = shared_state.borrow_mut();
                                state.connection = None;
                                state.metrics.is_connected = false;

                                if is_reliable {
                                    state.reliable_queue.push_front(data);
                                } else {
                                    state.metrics.record_message_dropped();
                                }

                                let unreliable_count = state.unreliable_queue.len();
                                for _ in 0..unreliable_count {
                                    state.metrics.record_message_dropped();
                                }
                                state.unreliable_queue.clear();
                            }
                            break;
                        }
                    }
                }
            }

            // Handle reading
            read_result = async {
                match &mut current_connection {
                    Some(stream) => {
                        let mut buffer = vec![0u8; 4096];
                        stream.read(&mut buffer).await.map(|n| (buffer, n))
                    }
                    None => std::future::pending().await
                }
            } => {
                match read_result {
                    Ok((_buffer, 0)) => {
                        // Connection closed
                        current_connection = None;
                        read_buffer.clear();
                        {
                            let mut state = shared_state.borrow_mut();
                            state.connection = None;
                            state.metrics.is_connected = false;
                        }
                        // For incoming connections, exit when closed
                        break;
                    }
                    Ok((buffer, n)) => {
                        read_buffer.extend_from_slice(&buffer[..n]);
                        tracing::debug!("incoming_connection_task: received {} bytes", n);

                        // Parse packets
                        loop {
                            if read_buffer.len() < HEADER_SIZE {
                                break;
                            }

                            match try_deserialize_packet(&read_buffer) {
                                Ok(Some((token, payload, consumed))) => {
                                    tracing::debug!(
                                        "incoming_connection_task: parsed packet token={}, payload={} bytes",
                                        token,
                                        payload.len()
                                    );

                                    {
                                        let mut state = shared_state.borrow_mut();
                                        state.metrics.record_message_received(consumed);
                                    }

                                    read_buffer.drain(..consumed);

                                    if receive_tx.send((token, payload)).is_err() {
                                        return;
                                    }
                                }
                                Ok(None) => {
                                    break;
                                }
                                Err(e) => {
                                    use crate::WireError;
                                    match &e {
                                        WireError::ChecksumMismatch { expected, actual } => {
                                            tracing::warn!(
                                                "ChecksumMismatch on incoming: expected={:#010x} actual={:#010x}",
                                                expected,
                                                actual
                                            );
                                        }
                                        _ => {
                                            tracing::warn!("Wire format error on incoming: {}", e);
                                        }
                                    }

                                    current_connection = None;
                                    read_buffer.clear();
                                    {
                                        let mut state = shared_state.borrow_mut();
                                        state.connection = None;
                                        state.metrics.is_connected = false;

                                        let unreliable_count = state.unreliable_queue.len();
                                        for _ in 0..unreliable_count {
                                            state.metrics.record_message_dropped();
                                        }
                                        state.unreliable_queue.clear();
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        current_connection = None;
                        read_buffer.clear();
                        {
                            let mut state = shared_state.borrow_mut();
                            state.connection = None;
                            state.metrics.is_connected = false;
                        }
                        // For incoming connections, exit on error
                        break;
                    }
                }
            }
        }
    }
}
