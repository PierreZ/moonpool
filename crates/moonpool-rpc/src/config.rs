//! Transport limits and settings.

use std::net::SocketAddr;
use std::time::Duration;

use crate::endpoint::Incarnation;
use crate::protocol::{
    HELLO_ENVELOPE_LEN, REJECTION_ENVELOPE_LEN, STREAM_END_ENVELOPE_LEN, request_envelope_len,
    stream_item_frame_len, stream_request_envelope_len,
};

/// The smallest accepted [`RpcConfig::max_frame_bytes`]: every fixed
/// envelope (handshake, empty request, rejection) must fit a frame.
pub const MIN_FRAME_BYTES: u32 = 64;

const _: () = assert!(
    request_envelope_len(0) <= MIN_FRAME_BYTES as usize
        && HELLO_ENVELOPE_LEN <= MIN_FRAME_BYTES as usize
        && REJECTION_ENVELOPE_LEN <= MIN_FRAME_BYTES as usize
        && stream_request_envelope_len(0) <= MIN_FRAME_BYTES as usize
        && STREAM_END_ENVELOPE_LEN <= MIN_FRAME_BYTES as usize,
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
/// Every queue the transport owns is bounded by one of these, by
/// [`ResourceLimits`] or by [`StreamPolicy`]; see [`ResourceLimits`] for
/// the whole table and what happens at each limit.
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
    /// Control frames (handshake, pings, pongs and admission rejections)
    /// one connection may queue. They are written before any data frame,
    /// so request, reply and stream traffic cannot starve them. Replies to
    /// admitted requests are not control frames: their room is reserved at
    /// admission ([`ResourceLimits::max_inflight_per_connection`]), and
    /// stream acknowledgements and cancellations are coalesced per stream.
    /// A peer whose pipelined rejected requests and pings exceed this
    /// reserve while this side cannot write is closed rather than answered
    /// silently; a peer with default limits (at most
    /// [`max_pending_calls`](Self::max_pending_calls) outstanding calls)
    /// cannot.
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
    /// Admission and buffering budgets per endpoint, per connection and
    /// per runtime.
    pub limits: ResourceLimits,
    /// Reply stream credit and stream budgets.
    pub streams: StreamPolicy,
}

/// Admission and buffering budgets: what one runtime admits, queues and
/// retains, per endpoint, per connection (peer) and in total.
///
/// Every budget is enforced where the work would enter, and a refusal
/// there is reported as [`ErrorReason::Overloaded`](crate::ErrorReason::Overloaded)
/// with [`Execution::NotAdmitted`](crate::Execution::NotAdmitted): nothing
/// was queued, sent or handed to a handler. Nothing already admitted is
/// dropped or failed to make room.
///
/// | Budget | Scope | Checked when | At the limit |
/// |---|---|---|---|
/// | [`RpcConfig::endpoint_queue_capacity`], [`endpoint_queue_bytes`](Self::endpoint_queue_bytes) | endpoint | a request is admitted | refused `Overloaded` |
/// | [`max_inflight_per_connection`](Self::max_inflight_per_connection) | connection | a two-way or stream request is admitted | refused `Overloaded` |
/// | [`max_inflight_requests`](Self::max_inflight_requests) | runtime | same | same |
/// | [`StreamPolicy::max_streams_per_connection`], [`StreamPolicy::max_streams`] | connection, runtime | a stream request is admitted | refused `Overloaded` |
/// | [`StreamPolicy::max_producer_bytes_per_connection`], [`StreamPolicy::max_producer_bytes`] | connection, runtime | a stream request is admitted (its window is reserved) | refused `Overloaded` |
/// | [`RpcConfig::max_queued_requests`], [`max_queued_bytes_per_connection`](Self::max_queued_bytes_per_connection) | connection | a caller queues a request | the call fails `Overloaded` |
/// | [`max_queued_bytes`](Self::max_queued_bytes) | runtime | same | same |
/// | [`RpcConfig::max_pending_calls`] | runtime | a call or stream starts | fails `Overloaded` |
/// | [`max_retained_bytes`](Self::max_retained_bytes) | runtime | a reliable call starts | fails `Overloaded` |
/// | [`StreamPolicy::max_buffered_bytes`] | runtime | a caller opens a stream | fails `Overloaded` |
/// | the stream's window | stream | the producer sends an item | the producer waits for credit |
/// | [`RpcConfig::reserved_control_frames`] | connection | a control frame is queued | the connection closes |
///
/// Replies to admitted requests and items of admitted streams are never
/// refused: an in-flight budget reserved their room at admission, and a
/// stream's window bounds its items. A slow writer therefore pushes back
/// on admission (new requests are refused, `NotAdmitted`) instead of
/// closing the session and failing unrelated calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Admitted requests (two-way and streams) arriving on one connection
    /// whose reply or stream end is still owed or queued unwritten.
    pub max_inflight_per_connection: usize,
    /// The same over every connection and local caller of the runtime.
    pub max_inflight_requests: usize,
    /// Bytes (whole frames) of requests queued unwritten on one connection
    /// above which new outgoing requests on it are refused. Replies and
    /// stream items are not counted: they are bounded by the in-flight
    /// budgets and the reserved stream windows, and a stream backlog to one
    /// slow peer never refuses calls to another.
    pub max_queued_bytes_per_connection: u64,
    /// The same over every connection.
    pub max_queued_bytes: u64,
    /// Request bodies retained for retransmission by reliable calls, in
    /// bytes, over the whole runtime.
    pub max_retained_bytes: u64,
    /// Encoded request bytes one endpoint may queue unread.
    pub endpoint_queue_bytes: u64,
    /// Frames one connection reads (or writes) in a batch before it yields
    /// to the other connections and tasks of its executor. Measured with
    /// the `stream_saturation` example: a unary call beside eight
    /// saturating streams waits about as long with 16 or 64 (the default),
    /// twice as long at the tail with 256, and a batch of 1 yields so often
    /// that the session crawls.
    pub max_frames_per_batch: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_inflight_per_connection: 1024,
            max_inflight_requests: 16 * 1024,
            max_queued_bytes_per_connection: 16 << 20,
            max_queued_bytes: 256 << 20,
            max_retained_bytes: 64 << 20,
            endpoint_queue_bytes: 16 << 20,
            max_frames_per_batch: 64,
        }
    }
}

impl ResourceLimits {
    fn validate(&self) -> Result<(), InvalidConfig> {
        let counts = [
            (
                "limits.max_inflight_per_connection",
                self.max_inflight_per_connection,
            ),
            ("limits.max_inflight_requests", self.max_inflight_requests),
            ("limits.max_frames_per_batch", self.max_frames_per_batch),
        ];
        let bytes = [
            (
                "limits.max_queued_bytes_per_connection",
                self.max_queued_bytes_per_connection,
            ),
            ("limits.max_queued_bytes", self.max_queued_bytes),
            ("limits.max_retained_bytes", self.max_retained_bytes),
            ("limits.endpoint_queue_bytes", self.endpoint_queue_bytes),
        ];
        if let Some((name, _)) = counts.iter().find(|(_, value)| *value == 0) {
            return Err(InvalidConfig(format!("{name} must be positive")));
        }
        if let Some((name, _)) = bytes.iter().find(|(_, value)| *value == 0) {
            return Err(InvalidConfig(format!("{name} must be positive")));
        }
        Ok(())
    }
}

/// Reply stream credit and stream budgets.
///
/// A caller announces a **window** when it opens a stream: the bytes it is
/// willing to hold received but not yet consumed by its application,
/// counted in accounted item sizes
/// ([`stream_item_frame_len`](crate::protocol::stream_item_frame_len): an
/// item's whole frame). The producer may have at most that many bytes sent
/// and unacknowledged, and the caller acknowledges an item only when its
/// application takes it, never merely because it was read off the socket
/// (`FoundationDB`'s `ReplyPromiseStream` acknowledgements).
///
/// The 1 MiB default window was measured with the `stream_saturation`
/// example on localhost: an eagerly consumed stream of 4 KiB items gains
/// throughput up to about 1 MiB and nothing beyond (`FoundationDB` uses
/// 2 MB, `RANGESTREAM_LIMIT_BYTES`). A stream costs its consumer at most
/// its window, so [`max_buffered_bytes`](Self::max_buffered_bytes) (256
/// MiB) admits 256 default streams per consuming runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamPolicy {
    /// The window this runtime announces for the streams it opens, unless
    /// the caller picks one
    /// ([`ServiceClient::get_reply_stream_with_window`](crate::ServiceClient::get_reply_stream_with_window)).
    /// An item larger than the window can never be sent: the producer is
    /// told so ([`SendError::TooLarge`](crate::SendError::TooLarge)).
    pub window_bytes: u64,
    /// The largest window this runtime honours as a producer; a caller
    /// announcing more is held to this.
    pub max_window_bytes: u64,
    /// Streams one connection may have open on this (producing) side.
    pub max_streams_per_connection: usize,
    /// Streams this runtime may produce at once, over every connection.
    pub max_streams: usize,
    /// The sum of the windows of the streams this runtime consumes at once:
    /// the most it may ever buffer for its own callers.
    pub max_buffered_bytes: u64,
    /// The sum of the windows of the streams produced over one connection:
    /// the most this runtime may have to queue for one peer's streams. A
    /// stream whose window does not fit is refused `Overloaded` before
    /// admission.
    pub max_producer_bytes_per_connection: u64,
    /// The same over every connection and local caller.
    pub max_producer_bytes: u64,
}

impl Default for StreamPolicy {
    fn default() -> Self {
        Self {
            window_bytes: 1 << 20,
            max_window_bytes: 16 << 20,
            max_streams_per_connection: 1024,
            max_streams: 16 * 1024,
            max_buffered_bytes: 256 << 20,
            max_producer_bytes_per_connection: 64 << 20,
            max_producer_bytes: 1 << 30,
        }
    }
}

impl StreamPolicy {
    fn validate(&self) -> Result<(), InvalidConfig> {
        let smallest = stream_item_frame_len(0);
        if self.window_bytes < smallest {
            return Err(InvalidConfig(format!(
                "streams.window_bytes below the {smallest}-byte smallest item"
            )));
        }
        if self.max_window_bytes < self.window_bytes {
            return Err(InvalidConfig(
                "streams.max_window_bytes below streams.window_bytes".into(),
            ));
        }
        if self.max_buffered_bytes < self.window_bytes {
            return Err(InvalidConfig(
                "streams.max_buffered_bytes below streams.window_bytes".into(),
            ));
        }
        if self.max_producer_bytes_per_connection < smallest
            || self.max_producer_bytes < self.max_producer_bytes_per_connection
        {
            return Err(InvalidConfig(
                "streams producer budgets must hold an item, per connection below per runtime"
                    .into(),
            ));
        }
        if self.max_streams == 0 || self.max_streams_per_connection == 0 {
            return Err(InvalidConfig("stream budgets must be positive".into()));
        }
        Ok(())
    }
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
    /// outstanding. With replies or streams still owed it is probed with a
    /// ping instead, and failed if nothing answers within `ping_timeout`
    /// (a dialer that vanished behind a half-open session), which releases
    /// the work owed to it. Its dialer pings, so this only reaps dead
    /// dialers; keep it well above `ping_interval`.
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
    /// Whether an accepted session may become this runtime's own
    /// connection to the peer that dialed it (see [`InboundSharing`]).
    pub share_inbound_sessions: InboundSharing,
    /// A runtime whose own dial to a peer has not established for this long
    /// adopts the peer's accepted session even when the address tie-break
    /// says to keep its own dial (`FoundationDB`'s `ALWAYS_ACCEPT_DELAY`),
    /// so a peer that can dial us but that we cannot dial stays reachable.
    pub always_accept_after: Duration,
}

/// When an accepted session may carry this runtime's own calls to its
/// dialer.
///
/// Each `Hello` may name the sender's listen address. With plaintext
/// sessions that claim is self-asserted: a runtime that adopts it routes
/// its calls to that address, including retained reliable requests, over
/// the claimant's connection, and counts the address as available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InboundSharing {
    /// Never share: announce no listen address, adopt nothing. Two
    /// runtimes calling each other then use one connection per direction.
    Disabled,
    /// Share, but adopt a session for address `X` only if its socket's
    /// peer IP is `X`'s IP (the default). This stops a process on another
    /// host from claiming a peer's identity; it does not stop another
    /// process on the claimed host, and it refuses peers behind NAT or with
    /// several addresses (they keep one connection per direction).
    /// Authenticated peer identity arrives with #218.
    #[default]
    SameIp,
    /// Share on the claim alone (`FoundationDB`'s behaviour). Only for
    /// networks where every process that can reach the listener is trusted.
    Trusted,
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
            share_inbound_sessions: InboundSharing::SameIp,
            always_accept_after: Duration::from_secs(15),
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
        if let Some((name, _)) = zero {
            return Err(InvalidConfig(format!("{name} must be positive")));
        }
        self.peer.validate()?;
        self.limits.validate()?;
        self.streams.validate()
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
            reserved_control_frames: 8 * 1024,
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(5),
            read_chunk_bytes: 16 * 1024,
            advertised_address: None,
            incarnation: None,
            peer: PeerPolicy::default(),
            limits: ResourceLimits::default(),
            streams: StreamPolicy::default(),
        }
    }
}
