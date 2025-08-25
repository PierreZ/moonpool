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
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(100),
            max_reconnect_delay: Duration::from_secs(30),
            max_queue_size: 1000,
            connection_timeout: Duration::from_secs(5),
            max_connection_failures: None, // Unlimited retries by default
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
        }
    }

    /// Create a configuration for low-latency local networking.
    pub fn local_network() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(10),
            max_reconnect_delay: Duration::from_secs(1),
            max_queue_size: 100,
            connection_timeout: Duration::from_millis(500),
            max_connection_failures: Some(10),
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
        }
    }
}
