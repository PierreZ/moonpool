//! Reconnecting client configuration and backoff policy.

use std::time::Duration;

use crate::config::KeepAlive;

/// How a [`ReconnectingChannel`](super::ReconnectingChannel) connects and
/// reconnects.
#[derive(Clone, Debug)]
pub struct ChannelConfig {
    /// Initial reconnect delay and connection-stability threshold.
    ///
    /// The delay doubles per consecutive failure up to
    /// [`max_reconnect_delay`](Self::max_reconnect_delay). A connection must
    /// remain alive for at least this duration to clear the failure count.
    pub initial_reconnect_delay: Duration,
    /// Maximum delay between reconnection attempts.
    pub max_reconnect_delay: Duration,
    /// Separate timeout applied to TCP connect and the HTTP/2 handshake.
    pub connection_timeout: Duration,
    /// Consecutive connection failures allowed before giving up indefinitely.
    /// `None` retries forever.
    pub max_connection_failures: Option<u32>,
    /// Optional HTTP/2 PING keepalive configuration.
    pub keep_alive: Option<KeepAlive>,
    /// Whether the connected stream supports efficient vectored writes.
    pub vectored_writes: bool,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(100),
            max_reconnect_delay: Duration::from_secs(30),
            connection_timeout: Duration::from_secs(5),
            max_connection_failures: None,
            keep_alive: None,
            vectored_writes: false,
        }
    }
}

/// Delay before the attempt following `failures` consecutive failures.
pub(super) fn backoff_delay(failures: u32, config: &ChannelConfig) -> Duration {
    if failures == 0 || config.initial_reconnect_delay.is_zero() {
        return Duration::ZERO;
    }

    let mut delay = config
        .initial_reconnect_delay
        .min(config.max_reconnect_delay);
    for _ in 1..failures {
        if delay == config.max_reconnect_delay {
            break;
        }
        delay = delay.saturating_mul(2).min(config.max_reconnect_delay);
    }
    delay
}
