//! Core peer implementation with automatic reconnection and message queuing.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use super::config::PeerConfig;
use super::error::{PeerError, PeerResult};
use super::metrics::PeerMetrics;
use crate::network::NetworkProvider;
use crate::sometimes_assert;
use crate::task::TaskProvider;
use crate::time::TimeProvider;

/// State for managing reconnections with exponential backoff.
#[derive(Debug, Clone)]
struct ReconnectState {
    /// Current backoff delay
    current_delay: Duration,

    /// Number of consecutive failures
    failure_count: u32,

    /// Time of last connection attempt
    last_attempt: Option<Instant>,

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
/// Follows FoundationDB's architecture: synchronous API with background actors.
pub struct Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    /// Shared state accessible to background actors
    shared_state: Rc<RefCell<PeerSharedState<N, T>>>,

    /// Trigger to wake writer actor when data is queued
    data_to_send: Rc<Notify>,

    /// Background actor handles
    writer_handle: Option<JoinHandle<()>>,

    /// Receive channel for incoming data
    receive_rx: mpsc::UnboundedReceiver<Vec<u8>>,

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

    /// Message queue for pending sends (writer actor drains this)
    send_queue: VecDeque<Vec<u8>>,

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
            send_queue: VecDeque::new(),
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
            receive_rx,
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

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.shared_state.borrow().connection.is_some()
    }

    /// Get current queue size.
    pub fn queue_size(&self) -> usize {
        self.shared_state.borrow().send_queue.len()
    }

    /// Get peer metrics.
    pub fn metrics(&self) -> PeerMetrics {
        self.shared_state.borrow().metrics.clone()
    }

    /// Get destination address.
    pub fn destination(&self) -> String {
        self.shared_state.borrow().destination.clone()
    }

    /// Send data to the peer.
    ///
    /// Queues the message immediately and triggers background writer.
    /// Returns without blocking on TCP I/O (matches FoundationDB pattern).
    pub fn send(&mut self, data: Vec<u8>) -> PeerResult<()> {
        // Queue message immediately (FoundationDB pattern)
        {
            let mut state = self.shared_state.borrow_mut();

            // Check queue capacity before adding
            sometimes_assert!(
                peer_queue_near_capacity,
                state.send_queue.len() >= (self.config.max_queue_size as f64 * 0.8) as usize,
                "Message queue should sometimes approach capacity limit"
            );

            // Handle queue overflow
            if state.send_queue.len() >= self.config.max_queue_size
                && state.send_queue.pop_front().is_some()
            {
                state.metrics.record_message_dropped();
            }

            let first_unsent = state.send_queue.is_empty();
            state.send_queue.push_back(data);
            state.metrics.record_message_queued();

            // Check if queue is growing with multiple messages
            sometimes_assert!(
                peer_queue_grows,
                state.send_queue.len() > 1,
                "Message queue should sometimes contain multiple messages"
            );

            // Wake connection task if this is first message (FoundationDB pattern)
            if first_unsent {
                self.data_to_send.notify_one();
            }
        }

        Ok(()) // Returns immediately - no async I/O!
    }

    /// Receive data from the peer.
    ///
    /// Waits for data from the background reader actor.
    pub async fn receive(&mut self) -> PeerResult<Vec<u8>> {
        self.receive_rx.recv().await.ok_or(PeerError::Disconnected)
    }

    /// Try to receive data from the peer without blocking.
    ///
    /// Returns immediately with available data or None if no data is ready.
    pub fn try_receive(&mut self) -> Option<Vec<u8>> {
        self.receive_rx.try_recv().ok()
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

    /// Close the connection and clear send queue.
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
        state.send_queue.clear();
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
async fn connection_task<
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
>(
    shared_state: Rc<RefCell<PeerSharedState<N, T>>>,
    data_to_send: Rc<Notify>,
    config: PeerConfig,
    receive_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
    _task_provider: TP,
) {
    let mut current_connection: Option<N::TcpStream> = None;

    loop {
        tokio::select! {
            // Check for shutdown
            _ = shutdown_rx.recv() => {
                tracing::trace!("Peer connection task shutting down");
                break;
            }

            // Wait for data to send (FoundationDB pattern)
            _ = data_to_send.notified() => {
                tracing::trace!("Peer connection task notified to send data");
                // First, ensure we have messages to send
                let has_messages = {
                    let state = shared_state.borrow();
                    !state.send_queue.is_empty()
                };

                if !has_messages {
                    tracing::trace!("Peer connection task: spurious wakeup, no messages to send");
                    continue; // Spurious wakeup, wait for real data
                }

                // Ensure we have a connection
                if current_connection.is_none() {
                    match establish_connection(&shared_state, &config).await {
                        Ok(stream) => {
                            current_connection = Some(stream);
                            {
                                let mut state = shared_state.borrow_mut();
                                state.connection = Some(()); // Mark as connected
                                state.metrics.is_connected = true;
                            }
                        }
                        Err(_) => {
                            continue; // Will retry on next notification
                        }
                    }
                }

                // Process send queue
                while let Some(ref mut stream) = current_connection {
                    // Get next message from queue
                    let message = {
                        let mut state = shared_state.borrow_mut();
                        state.send_queue.pop_front()
                    };

                    let Some(data) = message else {
                        break; // Queue empty
                    };

                    // Buggify: Sometimes force write failures to test requeuing
                    if crate::buggify_with_prob!(0.02) {
                        tracing::debug!("Buggify forcing write failure for requeue testing");
                        // Simulate write failure
                        current_connection = None;
                        {
                            let mut state = shared_state.borrow_mut();
                            state.connection = None;
                            state.metrics.is_connected = false;

                            sometimes_assert!(
                                peer_requeues_on_failure,
                                true,
                                "Peer should sometimes re-queue messages after send failure"
                            );

                            // Re-queue the failed message
                            state.send_queue.push_front(data);
                        }
                        break; // Exit send loop, will retry on next trigger
                    }

                    // Send the message (no RefCell borrow held)
                    match stream.write_all(&data).await {
                        Ok(_) => {
                            // Success
                            {
                                let mut state = shared_state.borrow_mut();
                                state.metrics.record_message_sent(data.len());
                                state.metrics.record_message_dequeued();
                            }
                        }
                        Err(_) => {
                            // Write failed - connection is bad
                            current_connection = None;
                            {
                                let mut state = shared_state.borrow_mut();
                                state.connection = None;
                                state.metrics.is_connected = false;

                                sometimes_assert!(
                                    peer_requeues_on_failure,
                                    true,
                                    "Peer should sometimes re-queue messages after send failure"
                                );

                                // Re-queue the failed message
                                state.send_queue.push_front(data);
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
                        tracing::trace!("Peer connection task: attempting to read from connection");
                        let mut buffer = vec![0u8; 4096];
                        stream.read(&mut buffer).await.map(|n| (buffer, n))
                    }
                    None => {
                        tracing::trace!("Peer connection task: no connection for reading");
                        std::future::pending().await  // Never resolves when no connection
                    }
                }
            } => {
                match read_result {
                    Ok((_buffer, 0)) => {
                        // Connection closed
                        current_connection = None;
                        {
                            let mut state = shared_state.borrow_mut();
                            state.connection = None;
                            state.metrics.is_connected = false;
                        }
                    }
                    Ok((buffer, n)) => {
                        // Data received
                        tracing::debug!("Peer connection task: received {} bytes", n);
                        let data = buffer[..n].to_vec();
                        {
                            let mut state = shared_state.borrow_mut();
                            state.metrics.record_message_received(n);
                        }

                        if receive_tx.send(data).is_err() {
                            tracing::debug!("Peer connection task: receiver dropped, shutting down");
                            return; // Receiver dropped
                        }
                    }
                    Err(_) => {
                        // Read error - connection likely broken
                        current_connection = None;
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
            let (should_backoff, delay) =
                if let Some(last_attempt) = state.reconnect_state.last_attempt {
                    let elapsed = last_attempt.elapsed();
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
