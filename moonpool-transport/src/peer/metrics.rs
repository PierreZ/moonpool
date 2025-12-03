//! Metrics collection and connection state tracking for peers.

use std::time::Duration;

/// Metrics and state information for a peer connection.
#[derive(Debug, Clone)]
pub struct PeerMetrics {
    /// Total number of connection attempts made
    pub connection_attempts: u64,

    /// Total number of successful connections established
    pub connections_established: u64,

    /// Total number of connection failures
    pub connection_failures: u64,

    /// Total number of messages sent successfully
    pub messages_sent: u64,

    /// Total number of messages received
    pub messages_received: u64,

    /// Total number of messages queued during disconnections
    pub messages_queued: u64,

    /// Total number of messages dropped due to queue overflow
    pub messages_dropped: u64,

    /// Total bytes sent over all connections
    pub bytes_sent: u64,

    /// Total bytes received over all connections
    pub bytes_received: u64,

    /// Current size of the send queue
    pub current_queue_size: usize,

    /// Time when the peer was created (simulation or wall time as Duration)
    pub created_at: Duration,

    /// Time of last successful connection (None if never connected)
    pub last_connected: Option<Duration>,

    /// Time of last connection failure (None if no failures)
    pub last_failure: Option<Duration>,

    /// Current consecutive failure count
    pub consecutive_failures: u32,

    /// Current reconnection delay
    pub current_reconnect_delay: Duration,

    /// Whether the peer is currently connected
    pub is_connected: bool,
}

impl Default for PeerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerMetrics {
    /// Create new metrics instance (defaults to zero creation time).
    pub fn new() -> Self {
        Self::new_at(Duration::ZERO)
    }

    /// Create new metrics instance with a specific creation time.
    pub fn new_at(created_at: Duration) -> Self {
        Self {
            connection_attempts: 0,
            connections_established: 0,
            connection_failures: 0,
            messages_sent: 0,
            messages_received: 0,
            messages_queued: 0,
            messages_dropped: 0,
            bytes_sent: 0,
            bytes_received: 0,
            current_queue_size: 0,
            created_at,
            last_connected: None,
            last_failure: None,
            consecutive_failures: 0,
            current_reconnect_delay: Duration::from_millis(100),
            is_connected: false,
        }
    }

    /// Record a connection attempt.
    pub fn record_connection_attempt(&mut self) {
        self.connection_attempts += 1;
    }

    /// Record a successful connection at a specific time.
    pub fn record_connection_success_at(&mut self, now: Duration) {
        self.connections_established += 1;
        self.last_connected = Some(now);
        self.consecutive_failures = 0;
        self.is_connected = true;
    }

    /// Record a connection failure at a specific time.
    pub fn record_connection_failure_at(&mut self, now: Duration, reconnect_delay: Duration) {
        self.connection_failures += 1;
        self.last_failure = Some(now);
        self.consecutive_failures += 1;
        self.current_reconnect_delay = reconnect_delay;
        self.is_connected = false;
    }

    /// Record a message sent.
    pub fn record_message_sent(&mut self, bytes: usize) {
        self.messages_sent += 1;
        self.bytes_sent += bytes as u64;
    }

    /// Record a message received.
    pub fn record_message_received(&mut self, bytes: usize) {
        self.messages_received += 1;
        self.bytes_received += bytes as u64;
    }

    /// Record a message queued.
    pub fn record_message_queued(&mut self) {
        self.messages_queued += 1;
        self.current_queue_size += 1;
    }

    /// Record a message dropped due to queue overflow.
    pub fn record_message_dropped(&mut self) {
        self.messages_dropped += 1;
    }

    /// Record a message dequeued (sent from queue).
    pub fn record_message_dequeued(&mut self) {
        if self.current_queue_size > 0 {
            self.current_queue_size -= 1;
        }
    }

    /// Calculate connection success rate as a percentage.
    pub fn connection_success_rate(&self) -> f64 {
        if self.connection_attempts == 0 {
            100.0
        } else {
            (self.connections_established as f64 / self.connection_attempts as f64) * 100.0
        }
    }

    /// Get the total uptime duration since creation.
    pub fn total_uptime(&self, now: Duration) -> Duration {
        now.saturating_sub(self.created_at)
    }

    /// Get time since last successful connection.
    pub fn time_since_last_connection(&self, now: Duration) -> Option<Duration> {
        self.last_connected.map(|t| now.saturating_sub(t))
    }

    /// Get time since last failure.
    pub fn time_since_last_failure(&self, now: Duration) -> Option<Duration> {
        self.last_failure.map(|t| now.saturating_sub(t))
    }
}
