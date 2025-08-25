//! Core peer implementation with automatic reconnection and message queuing.

use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::config::PeerConfig;
use super::error::{PeerError, PeerResult};
use super::metrics::PeerMetrics;
use crate::network::NetworkProvider;
use crate::sometimes_assert;
use crate::time::TimeProvider;
// use crate::sometimes_assert;

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
/// over NetworkProvider and TimeProvider implementations.
pub struct Peer<N: NetworkProvider, T: TimeProvider> {
    /// Network provider for creating connections
    network: N,

    /// Time provider for delays and timing
    time: T,

    /// Destination address
    destination: String,

    /// Current connection state
    connection: Option<N::TcpStream>,

    /// Message queue for pending sends (FIFO)
    send_queue: VecDeque<Vec<u8>>,

    /// Reconnection state management
    reconnect_state: ReconnectState,

    /// Configuration for behavior
    config: PeerConfig,

    /// Metrics collection
    metrics: PeerMetrics,
}

impl<N: NetworkProvider, T: TimeProvider> Peer<N, T> {
    /// Create a new peer for the destination address.
    pub fn new(network: N, time: T, destination: String, config: PeerConfig) -> Self {
        let reconnect_state = ReconnectState::new(config.initial_reconnect_delay);
        let now = time.now();

        Self {
            network,
            time,
            destination,
            connection: None,
            send_queue: VecDeque::new(),
            reconnect_state,
            config,
            metrics: PeerMetrics::new_at(now),
        }
    }

    /// Create a new peer with default configuration.
    pub fn new_with_defaults(network: N, time: T, destination: String) -> Self {
        Self::new(network, time, destination, PeerConfig::default())
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    /// Get current queue size.
    pub fn queue_size(&self) -> usize {
        self.send_queue.len()
    }

    /// Get peer metrics.
    pub fn metrics(&self) -> &PeerMetrics {
        &self.metrics
    }

    /// Get destination address.
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// Send data to the peer.
    ///
    /// If connected, sends immediately. If disconnected, queues the message
    /// and attempts to reconnect.
    pub async fn send(&mut self, data: Vec<u8>) -> PeerResult<()> {
        tracing::info!(
            "Peer::send called with {} bytes, connected={}, queue_size={}",
            data.len(),
            self.connection.is_some(),
            self.send_queue.len()
        );

        // If we have a connection, try to send immediately
        if self.connection.is_some() {
            tracing::info!("Peer::send attempting direct send (connected)");
            match self.send_raw(&data).await {
                Ok(_) => {
                    tracing::info!("Peer::send direct send successful");
                    self.metrics.record_message_sent(data.len());

                    // Verify direct send without queuing
                    sometimes_assert!(
                        peer_sends_without_queue,
                        self.send_queue.is_empty(),
                        "Peer should sometimes send directly without queuing"
                    );

                    return Ok(());
                }
                Err(e) => {
                    tracing::info!("Peer::send direct send failed: {:?}, will queue", e);
                    // Connection failed - it will be cleared in send_raw
                }
            }
        } else {
            tracing::info!("Peer::send no connection, will queue message");
        }

        // No connection or send failed - queue the message
        tracing::info!(
            "Peer::send queuing message, current queue size: {}",
            self.send_queue.len()
        );
        self.queue_message(data)?;
        tracing::info!(
            "Peer::send message queued, new queue size: {}",
            self.send_queue.len()
        );

        // Check if messages are being queued
        sometimes_assert!(
            peer_queues_messages,
            !self.send_queue.is_empty(),
            "Peer should sometimes queue messages when connection unavailable"
        );

        sometimes_assert!(
            peer_queue_grows,
            self.send_queue.len() > 1,
            "Message queue should sometimes contain multiple messages"
        );

        // Attempt to reconnect and process queue
        tracing::info!("Peer::send attempting to ensure connection and process queue");
        self.ensure_connection().await?;
        self.process_send_queue().await
    }

    /// Receive data from the peer.
    ///
    /// Attempts to reconnect if not currently connected.
    pub async fn receive(&mut self, buf: &mut [u8]) -> PeerResult<usize> {
        // Ensure we have a connection
        self.ensure_connection().await?;

        match &mut self.connection {
            Some(stream) => {
                match stream.read(buf).await {
                    Ok(n) => {
                        self.metrics.record_message_received(n);
                        Ok(n)
                    }
                    Err(e) => {
                        // Connection failed
                        self.connection = None;
                        self.metrics.record_connection_failure_at(
                            self.time.now(),
                            self.reconnect_state.current_delay,
                        );
                        Err(e.into())
                    }
                }
            }
            None => Err(PeerError::Disconnected),
        }
    }

    /// Force reconnection by dropping current connection and reconnecting.
    pub async fn reconnect(&mut self) -> PeerResult<()> {
        // Drop current connection
        self.connection = None;
        self.metrics.is_connected = false;

        // Reset reconnection state
        self.reconnect_state
            .reset(self.config.initial_reconnect_delay);

        // Attempt new connection
        self.connect().await
    }

    /// Close the connection and clear send queue.
    pub async fn close(&mut self) {
        self.connection = None;
        self.send_queue.clear();
        self.metrics.is_connected = false;
        self.metrics.current_queue_size = 0;
    }

    /// Add message to send queue with overflow handling.
    fn queue_message(&mut self, data: Vec<u8>) -> PeerResult<()> {
        // Check if queue is approaching capacity
        sometimes_assert!(
            peer_queue_near_capacity,
            self.send_queue.len() >= (self.config.max_queue_size as f64 * 0.8) as usize,
            "Message queue should sometimes approach capacity limit"
        );

        if self.send_queue.len() >= self.config.max_queue_size {
            // Drop oldest message (FIFO)
            if self.send_queue.pop_front().is_some() {
                self.metrics.record_message_dropped();
            }
        }

        self.send_queue.push_back(data);
        self.metrics.record_message_queued();
        Ok(())
    }

    /// Send raw data over the current connection.
    async fn send_raw(&mut self, data: &[u8]) -> PeerResult<()> {
        match &mut self.connection {
            Some(stream) => {
                match stream.write_all(data).await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        // Connection failed - clear it
                        self.connection = None;
                        self.metrics.record_connection_failure_at(
                            self.time.now(),
                            self.reconnect_state.current_delay,
                        );
                        Err(e.into())
                    }
                }
            }
            None => Err(PeerError::Disconnected),
        }
    }

    /// Ensure we have a valid connection, attempting to reconnect if necessary.
    async fn ensure_connection(&mut self) -> PeerResult<()> {
        if self.connection.is_some() {
            // Check connection reuse (only if we've already sent messages)
            if self.metrics.messages_sent > 0 {
                sometimes_assert!(
                    peer_reuses_connection,
                    true,
                    "Peer should sometimes reuse existing connections"
                );
            }
            return Ok(());
        }

        // Check if this is a reconnection attempt (only if we've had failures)
        if self.reconnect_state.failure_count > 0 {
            sometimes_assert!(
                peer_reconnects,
                true,
                "Peer should sometimes need to reconnect after failures"
            );
        }

        self.connect().await
    }

    /// Attempt to establish a connection with exponential backoff.
    async fn connect(&mut self) -> PeerResult<()> {
        // Check if we've exceeded maximum failure count
        if let Some(max_failures) = self.config.max_connection_failures {
            if self.reconnect_state.failure_count >= max_failures {
                sometimes_assert!(
                    peer_hits_max_failures,
                    true,
                    "Peer should sometimes hit maximum failure limit"
                );
                return Err(PeerError::ConnectionFailed);
            }
        }

        // Wait for backoff delay if needed
        if let Some(last_attempt) = self.reconnect_state.last_attempt {
            let elapsed = last_attempt.elapsed();
            if elapsed < self.reconnect_state.current_delay {
                let sleep_duration = self.reconnect_state.current_delay - elapsed;

                // Check backoff activation
                sometimes_assert!(
                    peer_backoff_activated,
                    sleep_duration > Duration::from_millis(0),
                    "Peer should sometimes wait for backoff before reconnecting"
                );

                // Use TimeProvider for simulation-aware sleep
                if self.time.sleep(sleep_duration).await.is_err() {
                    // Sleep failed - likely simulation shutdown, return error
                    return Err(PeerError::ConnectionFailed);
                }
            }
        }

        // Record attempt
        self.reconnect_state.last_attempt = Some(self.time.now());
        self.metrics.record_connection_attempt();

        // Attempt connection with timeout
        let connect_future = self.network.connect(&self.destination);

        match self
            .time
            .timeout(self.config.connection_timeout, connect_future)
            .await
        {
            Ok(Ok(Ok(stream))) => {
                // Connection successful

                // Check if this is recovery after failures
                sometimes_assert!(
                    peer_recovers_after_failures,
                    self.reconnect_state.failure_count > 0,
                    "Peer should sometimes successfully connect after previous failures"
                );

                self.connection = Some(stream);
                self.reconnect_state
                    .reset(self.config.initial_reconnect_delay);
                self.metrics.record_connection_success_at(self.time.now());
                Ok(())
            }
            Ok(Ok(Err(e))) => {
                // Connection failed
                self.handle_connection_failure();
                Err(e.into())
            }
            Ok(Err(())) => {
                // Timeout
                sometimes_assert!(
                    peer_connection_timeout,
                    true,
                    "Connection attempts should sometimes timeout"
                );
                self.handle_connection_failure();
                Err(PeerError::Timeout)
            }
            Err(_) => {
                // TimeProvider error (e.g., simulation shutdown)
                self.handle_connection_failure();
                Err(PeerError::ConnectionFailed)
            }
        }
    }

    /// Handle connection failure with exponential backoff.
    fn handle_connection_failure(&mut self) {
        self.connection = None;
        self.reconnect_state.failure_count += 1;

        // Exponential backoff with jitter
        let next_delay = std::cmp::min(
            self.reconnect_state.current_delay * 2,
            self.config.max_reconnect_delay,
        );

        // Check backoff behavior
        sometimes_assert!(
            peer_backoff_increases,
            next_delay > self.reconnect_state.current_delay,
            "Backoff delay should sometimes increase exponentially"
        );

        sometimes_assert!(
            peer_hits_max_backoff,
            next_delay == self.config.max_reconnect_delay,
            "Peer should sometimes hit maximum backoff delay"
        );

        self.reconnect_state.current_delay = next_delay;
        self.reconnect_state.reconnecting = true;

        self.metrics
            .record_connection_failure_at(self.time.now(), next_delay);
    }

    /// Process queued messages when connection is available.
    async fn process_send_queue(&mut self) -> PeerResult<()> {
        tracing::info!(
            "Peer::process_send_queue starting with {} queued messages",
            self.send_queue.len()
        );

        while let Some(data) = self.send_queue.pop_front() {
            tracing::info!(
                "Peer::process_send_queue processing message with {} bytes",
                data.len()
            );
            self.metrics.record_message_dequeued();

            match self.send_raw(&data).await {
                Ok(_) => {
                    tracing::info!("Peer::process_send_queue successfully sent queued message");
                    self.metrics.record_message_sent(data.len());
                    // Continue with next message
                }
                Err(e) => {
                    tracing::info!(
                        "Peer::process_send_queue failed to send queued message: {:?}, re-queuing",
                        e
                    );
                    // Connection failed - put message back at front and return error
                    sometimes_assert!(
                        peer_requeues_on_failure,
                        true,
                        "Peer should sometimes re-queue messages after send failure"
                    );

                    self.send_queue.push_front(data);
                    self.metrics.record_message_queued(); // Re-queue metrics
                    return Err(e);
                }
            }
        }

        tracing::info!("Peer::process_send_queue completed, queue is now empty");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SimWorld;

    #[tokio::test]
    async fn test_peer_creation() {
        let sim = SimWorld::new();
        let network = sim.network_provider();
        let time = sim.time_provider();
        let config = PeerConfig::default();

        let peer = Peer::new(network, time, "test:8080".to_string(), config);

        assert_eq!(peer.destination(), "test:8080");
        assert!(!peer.is_connected());
        assert_eq!(peer.queue_size(), 0);
    }

    #[tokio::test]
    async fn test_peer_with_defaults() {
        let sim = SimWorld::new();
        let network = sim.network_provider();
        let time = sim.time_provider();

        let peer = Peer::new_with_defaults(network, time, "test:8080".to_string());

        assert_eq!(peer.destination(), "test:8080");
        assert!(!peer.is_connected());
    }

    #[tokio::test]
    async fn test_peer_metrics_initialization() {
        let sim = SimWorld::new();
        let network = sim.network_provider();
        let time = sim.time_provider();

        let peer = Peer::new_with_defaults(network, time, "test:8080".to_string());
        let metrics = peer.metrics();

        assert_eq!(metrics.connection_attempts, 0);
        assert_eq!(metrics.connections_established, 0);
        assert_eq!(metrics.connection_failures, 0);
        assert!(!metrics.is_connected);
    }

    #[tokio::test]
    async fn test_config_presets() {
        let local_config = PeerConfig::local_network();
        assert_eq!(
            local_config.initial_reconnect_delay,
            Duration::from_millis(10)
        );
        assert_eq!(local_config.max_connection_failures, Some(10));

        let wan_config = PeerConfig::wan_network();
        assert_eq!(
            wan_config.initial_reconnect_delay,
            Duration::from_millis(500)
        );
        assert_eq!(wan_config.max_connection_failures, None);
    }
}
