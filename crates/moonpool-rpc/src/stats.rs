//! Observable counters and lifetime probes.

use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// A snapshot of one runtime's counters and gauges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RpcStats {
    /// Live registered endpoints.
    pub endpoints: usize,
    /// Calls awaiting a reply.
    pub pending_calls: usize,
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
    pub(crate) live_tasks: AtomicUsize,
    pub(crate) live_connections: AtomicUsize,
}

impl Counters {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self, endpoints: usize, pending_calls: usize) -> RpcStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        RpcStats {
            endpoints,
            pending_calls,
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
}

impl ResourceProbe {
    pub(crate) fn new(counters: Arc<Counters>, alive: Weak<()>) -> Self {
        Self { counters, alive }
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
