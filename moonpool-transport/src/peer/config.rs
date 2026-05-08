//! Configuration structures for peer behavior.

use std::time::Duration;

/// Configuration for connection health monitoring.
///
/// When enabled, the peer periodically sends ping packets to detect
/// unresponsive connections. Follows FoundationDB's `connectionMonitor`
/// pattern (`FlowTransport.actor.cpp:616-699`).
///
/// Monitoring is only active for outbound peers. Incoming peers
/// passively respond to pings but never initiate them.
#[derive(Clone, Debug)]
pub struct MonitorConfig {
    /// Interval between ping probe cycles.
    ///
    /// FDB equivalent: `FLOW_KNOBS->CONNECTION_MONITOR_LOOP_TIME`.
    pub ping_interval: Duration,

    /// Timeout waiting for a pong reply before considering the ping failed.
    ///
    /// If bytes were received since the ping was sent, the timeout is
    /// tolerated (connection is busy but alive). If no bytes were received,
    /// the connection is torn down.
    ///
    /// FDB equivalent: `FLOW_KNOBS->CONNECTION_MONITOR_TIMEOUT`.
    pub ping_timeout: Duration,

    /// Maximum consecutive timeouts tolerated when the connection is still
    /// receiving data (but pong replies are delayed).
    ///
    /// After this many consecutive timeouts with no byte-count change,
    /// the connection is torn down.
    pub max_tolerated_timeouts: u32,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(1),
            ping_timeout: Duration::from_secs(2),
            max_tolerated_timeouts: 3,
        }
    }
}

impl MonitorConfig {
    /// Create a monitor config tuned for local/low-latency networks.
    pub fn local_network() -> Self {
        Self {
            ping_interval: Duration::from_millis(500),
            ping_timeout: Duration::from_secs(1),
            max_tolerated_timeouts: 2,
        }
    }

    /// Create a monitor config tuned for WAN/high-latency networks.
    pub fn wan_network() -> Self {
        Self {
            ping_interval: Duration::from_secs(5),
            ping_timeout: Duration::from_secs(10),
            max_tolerated_timeouts: 5,
        }
    }
}

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

    /// Connection health monitoring configuration.
    ///
    /// When `Some`, the peer periodically pings to detect unresponsive
    /// connections (FDB `connectionMonitor` pattern). When `None` (default),
    /// no active health monitoring is performed.
    ///
    /// Only effective for outbound peers; incoming peers always respond
    /// to pings but never initiate them.
    pub monitor: Option<MonitorConfig>,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(100),
            max_reconnect_delay: Duration::from_secs(30),
            max_queue_size: 1000,
            connection_timeout: Duration::from_secs(5),
            max_connection_failures: None, // Unlimited retries by default
            monitor: Some(MonitorConfig::default()),
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
            monitor: Some(MonitorConfig::default()),
        }
    }

    /// Enable connection monitoring with the given configuration.
    pub fn with_monitor(mut self, config: MonitorConfig) -> Self {
        self.monitor = Some(config);
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
            monitor: Some(MonitorConfig::local_network()),
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
            monitor: Some(MonitorConfig::wan_network()),
        }
    }
}
