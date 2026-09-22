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
}

impl RpcConfig {
    /// Check the limits a runtime needs to function.
    ///
    /// # Errors
    ///
    /// [`InvalidConfig`] when `max_frame_bytes` is outside
    /// `MIN_FRAME_BYTES..=MAX_FRAME_BYTES` or a queue, budget or read size
    /// is zero.
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
            None => Ok(()),
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
        }
    }
}
