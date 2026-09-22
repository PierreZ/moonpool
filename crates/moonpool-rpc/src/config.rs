//! Transport limits and settings.

use std::time::Duration;

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
    /// requests are refused with [`RpcError::Overloaded`](crate::RpcError::Overloaded).
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
    /// Budget for establishing one outbound connection, on provider time.
    pub connect_timeout: Duration,
    /// Size of one socket read.
    pub read_chunk_bytes: usize,
    /// Address to advertise in endpoints instead of the listener's local
    /// address (for example when bound to a wildcard address).
    pub advertised_address: Option<String>,
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
            read_chunk_bytes: 16 * 1024,
            advertised_address: None,
        }
    }
}
