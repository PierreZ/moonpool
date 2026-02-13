//! Reference model for transport simulation testing.
//!
//! A BTreeMap-based ground truth that tracks all messages sent and received.
//! Invariants compare actual system behavior against this model.

use std::collections::{BTreeMap, BTreeSet};

/// Record of a single message in the reference model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    /// Sequence ID of the message.
    pub seq_id: u64,
    /// Size of the payload in bytes.
    pub payload_size: usize,
    /// Sender identifier.
    pub sender: String,
}

/// Reference model tracking all transport activity.
///
/// Uses BTreeMap for deterministic iteration order (FDB convention).
/// All fields are public for invariant access.
#[derive(Debug, Clone, Default)]
pub struct TransportRefModel {
    /// Reliable messages sent (keyed by seq_id).
    pub reliable_sent: BTreeMap<u64, MessageRecord>,
    /// Reliable messages received (keyed by seq_id).
    pub reliable_received: BTreeMap<u64, MessageRecord>,
    /// Unreliable messages sent (keyed by seq_id).
    pub unreliable_sent: BTreeMap<u64, MessageRecord>,
    /// Unreliable messages received (keyed by seq_id).
    pub unreliable_received: BTreeMap<u64, MessageRecord>,
    /// RPC requests sent (keyed by request_id).
    pub rpc_requests_sent: BTreeMap<u64, MessageRecord>,
    /// RPC responses received successfully (keyed by request_id).
    pub rpc_responses_received: BTreeMap<u64, MessageRecord>,
    /// RPC request IDs that resulted in BrokenPromise.
    pub rpc_broken_promises: BTreeSet<u64>,
    /// RPC request IDs that resulted in Timeout.
    pub rpc_timeouts: BTreeSet<u64>,
    /// RPC request IDs that resulted in ConnectionFailed.
    pub rpc_connection_failures: BTreeSet<u64>,
    /// Count of duplicate reliable messages received.
    pub duplicate_reliable_count: u64,
    /// Count of duplicate unreliable messages received.
    pub duplicate_unreliable_count: u64,
}

impl TransportRefModel {
    /// Create a new empty reference model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a reliable message being sent.
    pub fn record_reliable_sent(&mut self, seq_id: u64, payload_size: usize, sender: &str) {
        self.reliable_sent.insert(
            seq_id,
            MessageRecord {
                seq_id,
                payload_size,
                sender: sender.to_string(),
            },
        );
    }

    /// Record a reliable message being received.
    pub fn record_reliable_received(&mut self, seq_id: u64, payload_size: usize, sender: &str) {
        if self.reliable_received.contains_key(&seq_id) {
            self.duplicate_reliable_count += 1;
        }
        self.reliable_received.insert(
            seq_id,
            MessageRecord {
                seq_id,
                payload_size,
                sender: sender.to_string(),
            },
        );
    }

    /// Record an unreliable message being sent.
    pub fn record_unreliable_sent(&mut self, seq_id: u64, payload_size: usize, sender: &str) {
        self.unreliable_sent.insert(
            seq_id,
            MessageRecord {
                seq_id,
                payload_size,
                sender: sender.to_string(),
            },
        );
    }

    /// Record an unreliable message being received.
    pub fn record_unreliable_received(&mut self, seq_id: u64, payload_size: usize, sender: &str) {
        if self.unreliable_received.contains_key(&seq_id) {
            self.duplicate_unreliable_count += 1;
        }
        self.unreliable_received.insert(
            seq_id,
            MessageRecord {
                seq_id,
                payload_size,
                sender: sender.to_string(),
            },
        );
    }

    /// Record an RPC request being sent.
    pub fn record_rpc_sent(&mut self, request_id: u64, sender: &str) {
        self.rpc_requests_sent.insert(
            request_id,
            MessageRecord {
                seq_id: request_id,
                payload_size: 0,
                sender: sender.to_string(),
            },
        );
    }

    /// Record an RPC response being received successfully.
    pub fn record_rpc_response(&mut self, request_id: u64) {
        self.rpc_responses_received.insert(
            request_id,
            MessageRecord {
                seq_id: request_id,
                payload_size: 0,
                sender: String::new(),
            },
        );
    }

    /// Record an RPC broken promise.
    pub fn record_rpc_broken_promise(&mut self, request_id: u64) {
        self.rpc_broken_promises.insert(request_id);
    }

    /// Record an RPC timeout.
    pub fn record_rpc_timeout(&mut self, request_id: u64) {
        self.rpc_timeouts.insert(request_id);
    }

    /// Record an RPC connection failure.
    pub fn record_rpc_connection_failure(&mut self, request_id: u64) {
        self.rpc_connection_failures.insert(request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ref_model_reliable_tracking() {
        let mut model = TransportRefModel::new();
        model.record_reliable_sent(1, 100, "client");
        model.record_reliable_sent(2, 200, "client");
        model.record_reliable_received(1, 100, "client");

        assert_eq!(model.reliable_sent.len(), 2);
        assert_eq!(model.reliable_received.len(), 1);
        assert!(model.reliable_received.contains_key(&1));
    }

    #[test]
    fn test_ref_model_duplicate_detection() {
        let mut model = TransportRefModel::new();
        model.record_reliable_received(1, 100, "client");
        model.record_reliable_received(1, 100, "client");

        assert_eq!(model.duplicate_reliable_count, 1);
    }

    #[test]
    fn test_ref_model_rpc_tracking() {
        let mut model = TransportRefModel::new();
        model.record_rpc_sent(1, "client");
        model.record_rpc_sent(2, "client");
        model.record_rpc_sent(3, "client");

        model.record_rpc_response(1);
        model.record_rpc_broken_promise(2);
        model.record_rpc_timeout(3);

        assert_eq!(model.rpc_requests_sent.len(), 3);
        assert_eq!(model.rpc_responses_received.len(), 1);
        assert_eq!(model.rpc_broken_promises.len(), 1);
        assert_eq!(model.rpc_timeouts.len(), 1);
    }
}
