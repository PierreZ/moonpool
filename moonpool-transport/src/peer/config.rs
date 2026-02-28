//! Configuration structures for peer behavior.

use std::time::Duration;

/// Configuration for peer behavior and reconnection parameters.
#[derive(Clone, Debug)]
pub struct PeerConfig {
    /// Initial delay before attempting reconnection
    pub initial_reconnect_delay: Duration,

    /// Maximum delay between reconnection attempts
    pub max_reconnect_delay: Duration,

    /// Maximum number of messages to queue when disconnected
    pub max_queue_size: usize,

    /// Timeout for connection attempts
    pub connection_timeout: Duration,

    /// Maximum number of consecutive connection failures before giving up
    /// None means unlimited retries
    pub max_connection_failures: Option<u32>,

    /// Interval between ping health checks.
    ///
    /// FDB Reference: `CONNECTION_MONITOR_LOOP_TIME` (FlowTransport.actor.cpp:659)
    ///
    /// Set to `Duration::ZERO` to disable ping monitoring.
    pub ping_interval: Duration,

    /// Maximum time to wait for a pong response before declaring timeout.
    ///
    /// FDB Reference: `CONNECTION_MONITOR_TIMEOUT` (FlowTransport.actor.cpp:664)
    ///
    /// When no pong arrives within this duration, the connection is considered
    /// dead and the peer transitions to reconnecting state.
    pub ping_timeout: Duration,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(100),
            max_reconnect_delay: Duration::from_secs(30),
            max_queue_size: 1000,
            connection_timeout: Duration::from_secs(5),
            max_connection_failures: None, // Unlimited retries by default
            ping_interval: Duration::ZERO, // Disabled by default
            ping_timeout: Duration::from_secs(2),
        }
    }
}

impl PeerConfig {
    /// Create a new configuration with specified parameters.
    pub fn new(
        max_queue_size: usize,
        connection_timeout: Duration,
        initial_reconnect_delay: Duration,
        max_reconnect_delay: Duration,
        max_connection_failures: Option<u32>,
    ) -> Self {
        Self {
            initial_reconnect_delay,
            max_reconnect_delay,
            max_queue_size,
            connection_timeout,
            max_connection_failures,
            ping_interval: Duration::ZERO,
            ping_timeout: Duration::from_secs(2),
        }
    }

    /// Enable ping monitoring with the given interval and timeout.
    ///
    /// FDB Reference: connectionMonitor (FlowTransport.actor.cpp:616-699)
    pub fn with_ping(mut self, interval: Duration, timeout: Duration) -> Self {
        self.ping_interval = interval;
        self.ping_timeout = timeout;
        self
    }

    /// Create a configuration for low-latency local networking.
    pub fn local_network() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(10),
            max_reconnect_delay: Duration::from_secs(1),
            max_queue_size: 100,
            connection_timeout: Duration::from_millis(500),
            max_connection_failures: Some(10),
            ping_interval: Duration::from_millis(500),
            ping_timeout: Duration::from_secs(1),
        }
    }

    /// Create a configuration for high-latency WAN networking.
    pub fn wan_network() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(500),
            max_reconnect_delay: Duration::from_secs(60),
            max_queue_size: 5000,
            connection_timeout: Duration::from_secs(30),
            max_connection_failures: None, // Unlimited retries for WAN
            ping_interval: Duration::from_secs(2),
            ping_timeout: Duration::from_secs(5),
        }
    }
}
