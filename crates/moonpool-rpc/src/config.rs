//! Transport limits and settings.

use std::net::SocketAddr;
use std::time::Duration;

use crate::endpoint::Incarnation;
use crate::protocol::{HELLO_ENVELOPE_LEN, REJECTION_ENVELOPE_LEN, request_envelope_len};

/// The smallest accepted [`RpcConfig::max_frame_bytes`]: every fixed
/// envelope (handshake, empty request, rejection) must fit a frame.
pub const MIN_FRAME_BYTES: u32 = 64;

const _: () = assert!(
    request_envelope_len(0) <= MIN_FRAME_BYTES as usize
        && HELLO_ENVELOPE_LEN <= MIN_FRAME_BYTES as usize
        && REJECTION_ENVELOPE_LEN <= MIN_FRAME_BYTES as usize,
    "MIN_FRAME_BYTES must hold every fixed envelope"
);

/// The largest accepted [`RpcConfig::max_frame_bytes`] (1 GiB). Keeps
/// `header + length` arithmetic in range on 32-bit targets.
pub const MAX_FRAME_BYTES: u32 = 1 << 30;

/// An [`RpcConfig`] that cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid RPC configuration: {0}")]
pub struct InvalidConfig(pub String);

/// Hard limits and settings of one RPC runtime.
///
/// Every queue the transport owns is bounded by one of these. The numbers
/// are conservative defaults for the first package; the resource-control
/// package (#216) measures and freezes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcConfig {
    /// Largest frame payload accepted or produced, in bytes. A peer
    /// declaring a larger frame is a protocol violation; a local request
    /// or reply above it fails without being sent.
    pub max_frame_bytes: u32,
    /// Live endpoints this runtime may register.
    pub max_endpoints: usize,
    /// Admitted-but-unread requests one endpoint may queue; beyond it new
    /// requests are refused with [`ErrorReason::Overloaded`](crate::ErrorReason::Overloaded).
    pub endpoint_queue_capacity: usize,
    /// Calls this runtime may have awaiting a reply at once.
    pub max_pending_calls: usize,
    /// Open connections (inbound plus outbound) this runtime may hold.
    pub max_connections: usize,
    /// Request frames one connection may queue for writing.
    pub max_queued_requests: usize,
    /// Control frames (handshake, replies, rejections) one connection may
    /// queue, reserved separately so request traffic cannot starve them.
    /// A connection that would exceed it is closed rather than dropping a
    /// reply silently.
    pub reserved_control_frames: usize,
    /// Budget for connecting and upgrading one outbound connection, on
    /// provider time.
    pub connect_timeout: Duration,
    /// Budget for upgrading an accepted connection, and for either peer's
    /// `Hello` to arrive once a session runs.
    pub handshake_timeout: Duration,
    /// Size of one socket read.
    pub read_chunk_bytes: usize,
    /// Resolved address to advertise in endpoints instead of the
    /// listener's local address (for example when bound to a wildcard
    /// address).
    pub advertised_address: Option<SocketAddr>,
    /// The runtime's incarnation. `None` (the default) draws 128 fresh bits
    /// from the provider's random source; supply one only when the
    /// application owns a better source of uniqueness.
    pub incarnation: Option<Incarnation>,
    /// Reconnect, liveness, idle and failure-detection timing.
    pub peer: PeerPolicy,
}

/// How a runtime keeps, probes, abandons and re-opens connections to its
/// peers, and when it calls an address failed.
///
/// Plain data: every field is a tunable a simulation may push to an extreme
/// (the harness buggifies them per seed). All durations are provider
/// scheduling time. Defaults follow `FoundationDB`'s flow knobs
/// (`INITIAL_RECONNECTION_TIME`, `MAX_RECONNECTION_TIME`,
/// `RECONNECTION_TIME_GROWTH_RATE`, `RECONNECTION_RESET_TIME`,
/// `CONNECTION_MONITOR_LOOP_TIME`, `CONNECTION_MONITOR_TIMEOUT`,
/// `CONNECTION_MONITOR_IDLE_TIMEOUT`, `FAILURE_DETECTION_DELAY`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPolicy {
    /// First pause before re-dialing a peer whose connection ended.
    pub initial_reconnect_delay: Duration,
    /// Longest pause between dials to one peer.
    pub max_reconnect_delay: Duration,
    /// Growth of the pause after each connection that did not stay up, in
    /// percent (120 multiplies it by 1.2).
    pub reconnect_growth_percent: u32,
    /// A connection that stayed up at least this long resets the pause to
    /// `initial_reconnect_delay` when it ends.
    pub reconnect_reset_after: Duration,
    /// Every pause and liveness interval is scaled by a random factor in
    /// `1 ± jitter_percent / 100`.
    pub jitter_percent: u32,
    /// Interval between liveness pings on a peer connection.
    pub ping_interval: Duration,
    /// A peer connection that received nothing for this long after a ping
    /// is failed.
    pub ping_timeout: Duration,
    /// A peer connection carrying no call, queued frame or outstanding
    /// reply for this long is closed (not a failure).
    pub idle_timeout: Duration,
    /// An inbound connection this runtime does not use for its own calls
    /// is closed after receiving nothing for this long with no reply
    /// outstanding. Its dialer pings, so this only reaps dead dialers; keep
    /// it well above `ping_interval`.
    pub inbound_idle_timeout: Duration,
    /// Connection failures to an address must persist this long before the
    /// failure monitor marks the address failed. A disconnect is reported
    /// immediately either way.
    pub failure_detection_delay: Duration,
    /// Endpoint failures (destroyed endpoints, stale incarnations) the
    /// failure monitor remembers; the oldest is forgotten first. Forgetting
    /// one costs one more round trip, never a wrong answer: the server
    /// still rejects every stale reference.
    pub max_failed_endpoints: usize,
    /// Addresses the failure monitor tracks; available addresses without a
    /// live peer are forgotten first.
    pub max_tracked_addresses: usize,
    /// Use an accepted session as this runtime's own connection to the peer
    /// that dialed it (the listen address in its `Hello`), and settle
    /// simultaneous connects by address. With plaintext sessions that
    /// address is self-asserted: any process that can connect may claim to
    /// be a peer. Disable where unauthenticated peers can reach the
    /// listener; the security package (#218) ties it to an authenticated
    /// peer identity.
    pub share_inbound_sessions: bool,
}

impl Default for PeerPolicy {
    fn default() -> Self {
        Self {
            initial_reconnect_delay: Duration::from_millis(50),
            max_reconnect_delay: Duration::from_millis(500),
            reconnect_growth_percent: 120,
            reconnect_reset_after: Duration::from_secs(5),
            jitter_percent: 10,
            ping_interval: Duration::from_secs(1),
            ping_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_mins(3),
            inbound_idle_timeout: Duration::from_secs(216),
            failure_detection_delay: Duration::from_secs(4),
            max_failed_endpoints: 16 * 1024,
            max_tracked_addresses: 4096,
            share_inbound_sessions: true,
        }
    }
}

impl PeerPolicy {
    fn validate(&self) -> Result<(), InvalidConfig> {
        let nonzero = [
            ("peer.initial_reconnect_delay", self.initial_reconnect_delay),
            ("peer.ping_interval", self.ping_interval),
            ("peer.ping_timeout", self.ping_timeout),
            ("peer.idle_timeout", self.idle_timeout),
            ("peer.inbound_idle_timeout", self.inbound_idle_timeout),
        ];
        if let Some((name, _)) = nonzero.iter().find(|(_, value)| value.is_zero()) {
            return Err(InvalidConfig(format!("{name} must be positive")));
        }
        if self.max_reconnect_delay < self.initial_reconnect_delay {
            return Err(InvalidConfig(
                "peer.max_reconnect_delay below peer.initial_reconnect_delay".into(),
            ));
        }
        if self.reconnect_growth_percent < 100 {
            return Err(InvalidConfig(
                "peer.reconnect_growth_percent below 100 would shrink the backoff".into(),
            ));
        }
        if self.jitter_percent >= 100 {
            return Err(InvalidConfig(
                "peer.jitter_percent must be below 100".into(),
            ));
        }
        if self.inbound_idle_timeout <= self.ping_interval {
            return Err(InvalidConfig(
                "peer.inbound_idle_timeout must exceed peer.ping_interval".into(),
            ));
        }
        if self.max_failed_endpoints == 0 || self.max_tracked_addresses == 0 {
            return Err(InvalidConfig(
                "failure monitor capacities must be positive".into(),
            ));
        }
        Ok(())
    }
}

impl RpcConfig {
    /// Check the limits a runtime needs to function.
    ///
    /// # Errors
    ///
    /// [`InvalidConfig`] when `max_frame_bytes` is outside
    /// `MIN_FRAME_BYTES..=MAX_FRAME_BYTES` or a queue, budget or read size
    /// is zero, or when the [`PeerPolicy`] is inconsistent (a zero
    /// interval, a maximum below its minimum, a shrinking growth rate).
    pub fn validate(&self) -> Result<(), InvalidConfig> {
        if !(MIN_FRAME_BYTES..=MAX_FRAME_BYTES).contains(&self.max_frame_bytes) {
            return Err(InvalidConfig(format!(
                "max_frame_bytes {} outside {MIN_FRAME_BYTES}..={MAX_FRAME_BYTES}",
                self.max_frame_bytes
            )));
        }
        let zero = [
            ("max_endpoints", self.max_endpoints),
            ("endpoint_queue_capacity", self.endpoint_queue_capacity),
            ("max_pending_calls", self.max_pending_calls),
            ("max_connections", self.max_connections),
            ("max_queued_requests", self.max_queued_requests),
            ("reserved_control_frames", self.reserved_control_frames),
            ("read_chunk_bytes", self.read_chunk_bytes),
        ]
        .into_iter()
        .find(|(_, value)| *value == 0);
        match zero {
            Some((name, _)) => Err(InvalidConfig(format!("{name} must be positive"))),
            None => self.peer.validate(),
        }
    }
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1 << 20,
            max_endpoints: 4096,
            endpoint_queue_capacity: 256,
            max_pending_calls: 4096,
            max_connections: 512,
            max_queued_requests: 1024,
            reserved_control_frames: 1024,
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(5),
            read_chunk_bytes: 16 * 1024,
            advertised_address: None,
            incarnation: None,
            peer: PeerPolicy::default(),
        }
    }
}
