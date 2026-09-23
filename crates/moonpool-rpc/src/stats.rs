//! Observable counters and lifetime probes.

use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::security::{CredentialError, Denial};

/// A snapshot of one runtime's counters and gauges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RpcStats {
    /// Live registered endpoints.
    pub endpoints: usize,
    /// Calls awaiting a reply.
    pub pending_calls: usize,
    /// Reliable calls whose request is retained for retransmission.
    pub retained_calls: usize,
    /// Peer relationships (canonical remote addresses) tracked.
    pub peers: usize,
    /// Open connections.
    pub connections: usize,
    /// Request attempts started by callers of this runtime.
    pub calls_started: u64,
    /// Requests handed to a receiver (local or remote callers).
    pub requests_admitted: u64,
    /// Requests refused before reaching a receiver.
    pub requests_rejected: u64,
    /// Replies (including rejections) handed to a live route.
    pub replies_sent: u64,
    /// Replies whose route was gone (connection closed, caller runtime gone).
    pub replies_dropped: u64,
    /// Replies that arrived for a call no longer pending (caller cancelled).
    pub late_replies: u64,
    /// Replies that arrived on a different session than their call's.
    pub misrouted_replies: u64,
    /// Calls whose caller stopped waiting before completion.
    pub calls_abandoned: u64,
    /// Reply handles dropped without a reply.
    pub broken_promises: u64,
    /// Connections closed for a framing, checksum, envelope or handshake
    /// error.
    pub protocol_violations: u64,
    /// Of those, frames whose checksum did not match (corruption).
    pub checksum_failures: u64,
    /// Of those, peers that speak no supported protocol version.
    pub version_rejections: u64,
    /// Connections established (either direction).
    pub connections_opened: u64,
    /// Connections refused by the connection budget.
    pub connections_rejected: u64,
    /// Listener accept errors (transient and fatal).
    pub accept_errors: u64,
    /// Checksum mismatches on sessions that had completed their handshake
    /// (corruption of live traffic, as opposed to a bad opening).
    pub established_checksum_failures: u64,
    /// Calls that had begun transmission when this runtime closed their
    /// session for a checksum mismatch, and so failed as maybe-executed.
    pub calls_failed_by_corruption: u64,
    /// Reliable calls started (a subset of `calls_started`).
    pub reliable_calls_started: u64,
    /// Retained requests queued again on a new connection.
    pub retransmissions: u64,
    /// Retained requests released because their endpoint failed for good.
    pub retention_released_by_failure: u64,
    /// Calls refused locally because the failure monitor knew the endpoint was gone.
    pub calls_failed_fast: u64,
    /// One-way requests handed to a connection or a local receiver.
    pub one_way_sent: u64,
    /// One-way requests received from peers.
    pub one_way_received: u64,
    /// One-way requests dropped because they exceeded the peer's frame limit.
    pub one_way_dropped: u64,
    /// Reply handles finished with an explicit no-reply.
    pub explicit_no_replies: u64,
    /// Outbound connection attempts.
    pub dials: u64,
    /// Dials that first waited out the peer's reconnect backoff.
    pub reconnect_waits: u64,
    /// Accepted sessions adopted as the selected connection to their peer.
    pub adopted_connections: u64,
    /// Selected connections replaced by an adopted accepted session.
    pub replaced_connections: u64,
    /// Liveness pings sent.
    pub pings_sent: u64,
    /// Connections failed because nothing arrived after a ping.
    pub ping_timeouts: u64,
    /// Connections closed for idleness.
    pub idle_closes: u64,
    /// Accepted sessions whose claimed listen address failed the sharing check (served only).
    pub unverified_listen_addresses: u64,
    /// Accepted sessions adopted because this side's own dial had not established for `always_accept_after`.
    pub accepted_over_stalled_dial: u64,
    /// Accepted sessions served only because the peer table was full.
    pub peer_table_full: u64,
    /// Requests, calls or streams refused `Overloaded` by an in-flight, retention, stream or buffer budget (the endpoint queue and request-queue refusals are counted as rejections and call failures).
    pub overload_refusals: u64,
    /// Connections closed because their control reserve overflowed.
    pub control_reserve_closes: u64,
    /// Reply streams this runtime opened as a caller.
    pub streams_opened: u64,
    /// Reply streams this runtime admitted as a producer.
    pub streams_admitted: u64,
    /// Stream items queued by producers of this runtime.
    pub stream_items_sent: u64,
    /// Stream items refused as larger than their window or the frame limit.
    pub stream_items_refused: u64,
    /// Stream sends that had to wait for credit.
    pub stream_credit_waits: u64,
    /// Stream items acknowledged on arrival, handed to a consumer that was already waiting.
    pub stream_acks_immediate: u64,
    /// Stream items acknowledged when the application took them from the queue.
    pub stream_acks_popped: u64,
    /// Acknowledgements that returned credit to a producer of this runtime.
    pub stream_acks_received: u64,
    /// Stream acknowledgements and cancels ignored: repeated, or for a stream already over.
    pub stream_acks_ignored: u64,
    /// Stream protocol violations detected (sequence, window, item count or acknowledgement).
    pub stream_violations: u64,
    /// Streams ended by their producer (normal end, error or broken promise).
    pub streams_ended: u64,
    /// Produced streams stopped because their consumer abandoned them.
    pub streams_cancelled: u64,
    /// Consumed streams this runtime abandoned after its request may have left.
    pub streams_abandoned: u64,
    /// Produced streams ended because their connection ended.
    pub streams_disconnected: u64,
    /// Stream items or ends that arrived for a stream no longer consumed here.
    pub late_stream_frames: u64,
    /// Reply streams this runtime consumes right now.
    pub streams_consuming: usize,
    /// Reply streams this runtime produces right now.
    pub streams_producing: usize,
    /// Accounted stream bytes received and not yet taken by applications.
    pub stream_buffered_bytes: u64,
    /// Bytes of requests, replies and stream frames queued unwritten on every connection.
    pub queued_bytes: u64,
    /// Admitted requests whose reply or stream end is owed or queued.
    pub inflight_requests: usize,
    /// Sum of the windows of the streams this runtime consumes.
    pub stream_window_reserved: u64,
    /// Sum of the windows of the streams this runtime produces (reserved
    /// from the producer budgets).
    pub producer_window_reserved: u64,
    /// Of `queued_bytes`, the bytes of queued requests (what the queue
    /// budgets refuse).
    pub queued_request_bytes: u64,
    /// Requests whose credential verified into a principal.
    pub requests_authenticated: u64,
    /// Requests refused as unauthenticated (every [`CredentialError`](crate::security::CredentialError)).
    pub requests_unauthenticated: u64,
    /// Requests refused by the access policy (authenticated, not allowed).
    pub requests_permission_denied: u64,
    /// Accepted connections refused by the address allow list.
    pub connections_refused_by_policy: u64,
    /// Credentials this runtime attached to its own requests.
    pub credentials_attached: u64,
    /// Requests refused because a graceful shutdown was under way.
    pub shutdown_refusals: u64,
    /// Calls and streams a graceful shutdown ended at its deadline.
    pub calls_ended_by_shutdown: u64,
}

/// The runtime's counters, shared by everything that updates them.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) calls_started: AtomicU64,
    pub(crate) requests_admitted: AtomicU64,
    pub(crate) requests_rejected: AtomicU64,
    pub(crate) replies_sent: AtomicU64,
    pub(crate) replies_dropped: AtomicU64,
    pub(crate) late_replies: AtomicU64,
    pub(crate) misrouted_replies: AtomicU64,
    pub(crate) calls_abandoned: AtomicU64,
    pub(crate) broken_promises: AtomicU64,
    pub(crate) protocol_violations: AtomicU64,
    pub(crate) checksum_failures: AtomicU64,
    pub(crate) version_rejections: AtomicU64,
    pub(crate) connections_opened: AtomicU64,
    pub(crate) connections_rejected: AtomicU64,
    pub(crate) accept_errors: AtomicU64,
    pub(crate) established_checksum_failures: AtomicU64,
    pub(crate) calls_failed_by_corruption: AtomicU64,
    pub(crate) reliable_calls_started: AtomicU64,
    pub(crate) retransmissions: AtomicU64,
    pub(crate) retention_released_by_failure: AtomicU64,
    pub(crate) calls_failed_fast: AtomicU64,
    pub(crate) one_way_sent: AtomicU64,
    pub(crate) one_way_received: AtomicU64,
    pub(crate) one_way_dropped: AtomicU64,
    pub(crate) explicit_no_replies: AtomicU64,
    pub(crate) dials: AtomicU64,
    pub(crate) reconnect_waits: AtomicU64,
    pub(crate) adopted_connections: AtomicU64,
    pub(crate) replaced_connections: AtomicU64,
    pub(crate) pings_sent: AtomicU64,
    pub(crate) ping_timeouts: AtomicU64,
    pub(crate) idle_closes: AtomicU64,
    pub(crate) unverified_listen_addresses: AtomicU64,
    pub(crate) accepted_over_stalled_dial: AtomicU64,
    pub(crate) peer_table_full: AtomicU64,
    pub(crate) overload_refusals: AtomicU64,
    pub(crate) control_reserve_closes: AtomicU64,
    pub(crate) streams_opened: AtomicU64,
    pub(crate) streams_admitted: AtomicU64,
    pub(crate) stream_items_sent: AtomicU64,
    pub(crate) stream_items_refused: AtomicU64,
    pub(crate) stream_credit_waits: AtomicU64,
    pub(crate) stream_acks_immediate: AtomicU64,
    pub(crate) stream_acks_popped: AtomicU64,
    pub(crate) stream_acks_received: AtomicU64,
    pub(crate) stream_acks_ignored: AtomicU64,
    pub(crate) stream_violations: AtomicU64,
    pub(crate) streams_ended: AtomicU64,
    pub(crate) streams_cancelled: AtomicU64,
    pub(crate) streams_abandoned: AtomicU64,
    pub(crate) streams_disconnected: AtomicU64,
    pub(crate) late_stream_frames: AtomicU64,
    pub(crate) live_server_streams: AtomicUsize,
    pub(crate) stream_buffered_bytes: AtomicU64,
    pub(crate) queued_bytes: AtomicU64,
    pub(crate) inflight_requests: AtomicUsize,
    pub(crate) stream_window_reserved: AtomicU64,
    pub(crate) producer_window_reserved: AtomicU64,
    pub(crate) queued_request_bytes: AtomicU64,
    pub(crate) live_tasks: AtomicUsize,
    pub(crate) live_connections: AtomicUsize,
    pub(crate) requests_authenticated: AtomicU64,
    pub(crate) requests_permission_denied: AtomicU64,
    /// Unauthenticated refusals by [`CredentialError`] code (index code − 1):
    /// a fixed, bounded label set.
    pub(crate) denials_by_reason: [AtomicU64; crate::security::CREDENTIAL_ERRORS],
    pub(crate) connections_refused_by_policy: AtomicU64,
    pub(crate) credentials_attached: AtomicU64,
    pub(crate) shutdown_refusals: AtomicU64,
    pub(crate) calls_ended_by_shutdown: AtomicU64,
}

impl Counters {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one security refusal.
    pub(crate) fn count_denial(&self, denial: Denial) {
        match denial {
            Denial::Unauthenticated(reason) => {
                if let Some(counter) = self
                    .denials_by_reason
                    .get(usize::from(reason.code()).saturating_sub(1))
                {
                    Self::bump(counter);
                }
            }
            Denial::PermissionDenied => Self::bump(&self.requests_permission_denied),
        }
    }

    /// Unauthenticated refusals with `reason`.
    pub(crate) fn denials(&self, reason: CredentialError) -> u64 {
        self.denials_by_reason
            .get(usize::from(reason.code()).saturating_sub(1))
            .map_or(0, |counter| counter.load(Ordering::Relaxed))
    }

    pub(crate) fn snapshot(
        &self,
        endpoints: usize,
        pending_calls: usize,
        retained_calls: usize,
    ) -> RpcStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        RpcStats {
            endpoints,
            pending_calls,
            retained_calls,
            peers: 0,
            connections: self.live_connections.load(Ordering::Relaxed),
            calls_started: load(&self.calls_started),
            requests_admitted: load(&self.requests_admitted),
            requests_rejected: load(&self.requests_rejected),
            replies_sent: load(&self.replies_sent),
            replies_dropped: load(&self.replies_dropped),
            late_replies: load(&self.late_replies),
            misrouted_replies: load(&self.misrouted_replies),
            calls_abandoned: load(&self.calls_abandoned),
            broken_promises: load(&self.broken_promises),
            protocol_violations: load(&self.protocol_violations),
            checksum_failures: load(&self.checksum_failures),
            version_rejections: load(&self.version_rejections),
            connections_opened: load(&self.connections_opened),
            connections_rejected: load(&self.connections_rejected),
            accept_errors: load(&self.accept_errors),
            established_checksum_failures: load(&self.established_checksum_failures),
            calls_failed_by_corruption: load(&self.calls_failed_by_corruption),
            reliable_calls_started: load(&self.reliable_calls_started),
            retransmissions: load(&self.retransmissions),
            retention_released_by_failure: load(&self.retention_released_by_failure),
            calls_failed_fast: load(&self.calls_failed_fast),
            one_way_sent: load(&self.one_way_sent),
            one_way_received: load(&self.one_way_received),
            one_way_dropped: load(&self.one_way_dropped),
            explicit_no_replies: load(&self.explicit_no_replies),
            dials: load(&self.dials),
            reconnect_waits: load(&self.reconnect_waits),
            adopted_connections: load(&self.adopted_connections),
            replaced_connections: load(&self.replaced_connections),
            pings_sent: load(&self.pings_sent),
            ping_timeouts: load(&self.ping_timeouts),
            idle_closes: load(&self.idle_closes),
            unverified_listen_addresses: load(&self.unverified_listen_addresses),
            accepted_over_stalled_dial: load(&self.accepted_over_stalled_dial),
            peer_table_full: load(&self.peer_table_full),
            overload_refusals: load(&self.overload_refusals),
            control_reserve_closes: load(&self.control_reserve_closes),
            streams_opened: load(&self.streams_opened),
            streams_admitted: load(&self.streams_admitted),
            stream_items_sent: load(&self.stream_items_sent),
            stream_items_refused: load(&self.stream_items_refused),
            stream_credit_waits: load(&self.stream_credit_waits),
            stream_acks_immediate: load(&self.stream_acks_immediate),
            stream_acks_popped: load(&self.stream_acks_popped),
            stream_acks_received: load(&self.stream_acks_received),
            stream_acks_ignored: load(&self.stream_acks_ignored),
            stream_violations: load(&self.stream_violations),
            streams_ended: load(&self.streams_ended),
            streams_cancelled: load(&self.streams_cancelled),
            streams_abandoned: load(&self.streams_abandoned),
            streams_disconnected: load(&self.streams_disconnected),
            late_stream_frames: load(&self.late_stream_frames),
            streams_consuming: 0,
            streams_producing: self.live_server_streams.load(Ordering::Relaxed),
            stream_buffered_bytes: self.stream_buffered_bytes.load(Ordering::Relaxed),
            queued_bytes: self.queued_bytes.load(Ordering::Relaxed),
            inflight_requests: self.inflight_requests.load(Ordering::Relaxed),
            stream_window_reserved: self.stream_window_reserved.load(Ordering::Relaxed),
            producer_window_reserved: self.producer_window_reserved.load(Ordering::Relaxed),
            queued_request_bytes: self.queued_request_bytes.load(Ordering::Relaxed),
            requests_authenticated: load(&self.requests_authenticated),
            requests_unauthenticated: self.denials_by_reason.iter().map(load).sum(),
            requests_permission_denied: load(&self.requests_permission_denied),
            connections_refused_by_policy: load(&self.connections_refused_by_policy),
            credentials_attached: load(&self.credentials_attached),
            shutdown_refusals: load(&self.shutdown_refusals),
            calls_ended_by_shutdown: load(&self.calls_ended_by_shutdown),
        }
    }
}

/// Counts one driver-owned child future for as long as it exists.
pub(crate) struct TaskGuard(Arc<Counters>);

impl TaskGuard {
    pub(crate) fn new(counters: &Arc<Counters>) -> Self {
        counters.live_tasks.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(counters))
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.live_tasks.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A drop probe that outlives the runtime it watches.
///
/// Obtained from [`RpcHandle::probe`](crate::RpcHandle::probe) while the
/// runtime is alive, it keeps answering after the driver is dropped, so
/// tests can prove every child future, connection and the runtime state
/// itself were released.
#[derive(Debug, Clone)]
pub struct ResourceProbe {
    counters: Arc<Counters>,
    alive: Weak<()>,
    watch: Weak<crate::failure::watch::Watch>,
}

impl ResourceProbe {
    pub(crate) fn new(
        counters: Arc<Counters>,
        alive: Weak<()>,
        watch: Weak<crate::failure::watch::Watch>,
    ) -> Self {
        Self {
            counters,
            alive,
            watch,
        }
    }

    /// Failure-monitor handles and waits still holding the runtime's change
    /// latch (the runtime itself holds one while it runs). After shutdown
    /// every wait resolves with an error at its next poll; a non-zero count
    /// then only means a caller still owns such a handle or future.
    #[must_use]
    pub fn monitor_waiters(&self) -> usize {
        self.watch.strong_count()
    }

    /// Driver-owned child futures (accept loop, connection drivers) that
    /// still exist.
    #[must_use]
    pub fn live_tasks(&self) -> usize {
        self.counters.live_tasks.load(Ordering::Relaxed)
    }

    /// Connection objects that still exist.
    #[must_use]
    pub fn live_connections(&self) -> usize {
        self.counters.live_connections.load(Ordering::Relaxed)
    }

    /// Whether the runtime's shared state still exists.
    #[must_use]
    pub fn runtime_alive(&self) -> bool {
        self.alive.strong_count() > 0
    }

    /// Whether everything the runtime owned has been released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        !self.runtime_alive() && self.live_tasks() == 0 && self.live_connections() == 0
    }
}
