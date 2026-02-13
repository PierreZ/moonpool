//! Reference model and invariant checking for Peer E2E simulation testing.
//!
//! Tracks sent/received messages and validates Peer delivery guarantees
//! using BTreeMap-based deterministic state tracking (FDB pattern).

use std::collections::{BTreeMap, BTreeSet};

/// Record of a tracked message.
#[derive(Debug, Clone)]
pub struct MessageRecord {
    /// Whether the message was sent reliably.
    pub reliable: bool,
    /// Payload size in bytes.
    pub payload_size: usize,
}

/// Reference model for Peer-level E2E testing.
///
/// Tracks all sent and received messages to validate delivery guarantees.
#[derive(Debug, Default, Clone)]
pub struct PeerRefModel {
    /// Reliable messages sent (seq_id -> record).
    pub reliable_sent: BTreeMap<u64, MessageRecord>,
    /// Reliable messages received (seq_id -> record).
    pub reliable_received: BTreeMap<u64, MessageRecord>,
    /// Unreliable messages sent (seq_id -> record).
    pub unreliable_sent: BTreeMap<u64, MessageRecord>,
    /// Unreliable messages received (seq_id -> record).
    pub unreliable_received: BTreeMap<u64, MessageRecord>,
    /// Number of duplicate reliable messages received.
    pub reliable_duplicates: u64,
    /// Number of duplicate unreliable messages received.
    pub unreliable_duplicates: u64,
    /// Number of forced reconnects.
    pub reconnects: u64,
    /// Number of send failures.
    pub send_failures: u64,
    /// Sequence IDs where receive failures occurred.
    pub receive_failures: BTreeSet<u64>,
    /// Total operations executed.
    pub total_ops: u64,
}

impl PeerRefModel {
    /// Create a new empty reference model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a reliable message sent.
    pub fn record_reliable_sent(&mut self, seq_id: u64, payload_size: usize) {
        self.reliable_sent.insert(
            seq_id,
            MessageRecord {
                reliable: true,
                payload_size,
            },
        );
    }

    /// Record a reliable message received.
    pub fn record_reliable_received(&mut self, seq_id: u64, payload_size: usize) {
        if self.reliable_received.contains_key(&seq_id) {
            self.reliable_duplicates += 1;
        }
        self.reliable_received.insert(
            seq_id,
            MessageRecord {
                reliable: true,
                payload_size,
            },
        );
    }

    /// Record an unreliable message sent.
    pub fn record_unreliable_sent(&mut self, seq_id: u64, payload_size: usize) {
        self.unreliable_sent.insert(
            seq_id,
            MessageRecord {
                reliable: false,
                payload_size,
            },
        );
    }

    /// Record an unreliable message received.
    pub fn record_unreliable_received(&mut self, seq_id: u64, payload_size: usize) {
        if self.unreliable_received.contains_key(&seq_id) {
            self.unreliable_duplicates += 1;
        }
        self.unreliable_received.insert(
            seq_id,
            MessageRecord {
                reliable: false,
                payload_size,
            },
        );
    }

    /// Record a forced reconnect.
    pub fn record_reconnect(&mut self) {
        self.reconnects += 1;
    }

    /// Record a send failure.
    pub fn record_send_failure(&mut self) {
        self.send_failures += 1;
    }
}

/// Check always-true invariants on the reference model.
///
/// These invariants must hold regardless of chaos injection.
pub fn check_always_invariants(model: &PeerRefModel) {
    // No phantom reliable: received ⊆ sent
    for seq_id in model.reliable_received.keys() {
        moonpool_sim::assert_always!(
            model.reliable_sent.contains_key(seq_id),
            &format!(
                "Phantom reliable message: received seq_id={} that was never sent",
                seq_id
            )
        );
    }

    // No phantom unreliable: received ⊆ sent
    for seq_id in model.unreliable_received.keys() {
        moonpool_sim::assert_always!(
            model.unreliable_sent.contains_key(seq_id),
            &format!(
                "Phantom unreliable message: received seq_id={} that was never sent",
                seq_id
            )
        );
    }

    // Unreliable conservation: |received| <= |sent|
    moonpool_sim::assert_always!(
        model.unreliable_received.len() <= model.unreliable_sent.len(),
        &format!(
            "Unreliable conservation violated: received {} > sent {}",
            model.unreliable_received.len(),
            model.unreliable_sent.len()
        )
    );

    // Reliable conservation: |received| <= |sent|
    moonpool_sim::assert_always!(
        model.reliable_received.len() <= model.reliable_sent.len(),
        &format!(
            "Reliable conservation violated: received {} > sent {}",
            model.reliable_received.len(),
            model.reliable_sent.len()
        )
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ref_model_reliable_tracking() {
        let mut model = PeerRefModel::new();
        model.record_reliable_sent(1, 100);
        model.record_reliable_sent(2, 200);
        model.record_reliable_received(1, 100);

        assert_eq!(model.reliable_sent.len(), 2);
        assert_eq!(model.reliable_received.len(), 1);
        assert_eq!(model.reliable_duplicates, 0);
    }

    #[test]
    fn test_ref_model_duplicate_detection() {
        let mut model = PeerRefModel::new();
        model.record_reliable_sent(1, 100);
        model.record_reliable_received(1, 100);
        model.record_reliable_received(1, 100); // duplicate

        assert_eq!(model.reliable_duplicates, 1);
    }

    #[test]
    fn test_ref_model_unreliable_tracking() {
        let mut model = PeerRefModel::new();
        model.record_unreliable_sent(100, 50);
        model.record_unreliable_sent(101, 60);
        model.record_unreliable_received(100, 50);

        assert_eq!(model.unreliable_sent.len(), 2);
        assert_eq!(model.unreliable_received.len(), 1);
    }
}
