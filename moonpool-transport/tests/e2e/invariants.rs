//! Invariant tracking for Peer E2E tests.
//!
//! Tracks message delivery to verify correctness properties:
//! - Reliable messages are eventually delivered
//! - Unreliable messages may be dropped but never duplicated
//! - Message ordering is preserved
//! - No phantom messages (received ⊆ sent)

use std::collections::{HashMap, HashSet};

use moonpool_sim::{assert_always, assert_sometimes};

/// Tracks sent and received messages for invariant validation.
#[derive(Debug, Default, Clone)]
pub struct MessageInvariants {
    /// Sequence IDs of messages sent reliably (should all be delivered).
    pub reliable_sent: HashSet<u64>,
    /// Sequence IDs of messages received that were sent reliably.
    pub reliable_received: HashSet<u64>,

    /// Sequence IDs of messages sent unreliably (may be dropped).
    pub unreliable_sent: HashSet<u64>,
    /// Sequence IDs of messages received that were sent unreliably.
    pub unreliable_received: HashSet<u64>,

    /// Track order of reliable messages sent.
    pub reliable_sent_order: Vec<u64>,
    /// Track order of reliable messages received.
    pub reliable_received_order: Vec<u64>,

    /// Track order of unreliable messages sent.
    pub unreliable_sent_order: Vec<u64>,
    /// Track order of unreliable messages received.
    pub unreliable_received_order: Vec<u64>,

    /// Count of duplicate receives (should be 0).
    pub duplicate_count: u64,
}

impl MessageInvariants {
    /// Create a new empty invariant tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a message being sent.
    pub fn record_sent(&mut self, sequence_id: u64, reliable: bool) {
        if reliable {
            self.reliable_sent.insert(sequence_id);
            self.reliable_sent_order.push(sequence_id);
        } else {
            self.unreliable_sent.insert(sequence_id);
            self.unreliable_sent_order.push(sequence_id);
        }
    }

    /// Record a message being received. Returns true if duplicate.
    pub fn record_received(&mut self, sequence_id: u64, sent_reliably: bool) -> bool {
        let is_duplicate = if sent_reliably {
            let is_dup = self.reliable_received.contains(&sequence_id);
            self.reliable_received.insert(sequence_id);
            self.reliable_received_order.push(sequence_id);
            is_dup
        } else {
            let is_dup = self.unreliable_received.contains(&sequence_id);
            self.unreliable_received.insert(sequence_id);
            self.unreliable_received_order.push(sequence_id);
            is_dup
        };

        if is_duplicate {
            self.duplicate_count += 1;
        }

        is_duplicate
    }

    /// Validate invariants that must always hold.
    pub fn validate_always(&self) {
        // No duplicates allowed
        assert_always!(
            self.duplicate_count == 0,
            format!(
                "Message duplication detected: {} duplicates",
                self.duplicate_count
            )
        );

        // All received must be subset of sent (no phantom messages)
        for seq_id in &self.reliable_received {
            assert_always!(
                self.reliable_sent.contains(seq_id),
                format!("Received reliable message {} that was never sent", seq_id)
            );
        }

        for seq_id in &self.unreliable_received {
            assert_always!(
                self.unreliable_sent.contains(seq_id),
                format!("Received unreliable message {} that was never sent", seq_id)
            );
        }

        // Unreliable received <= unreliable sent
        assert_always!(
            self.unreliable_received.len() <= self.unreliable_sent.len(),
            format!(
                "Unreliable received ({}) > sent ({})",
                self.unreliable_received.len(),
                self.unreliable_sent.len()
            )
        );
    }

    /// Validate invariants for a receiver that only sees receives.
    pub fn validate_receiver_always(&self) {
        assert_sometimes!(
            self.duplicate_count > 0,
            "Receiver should sometimes see duplicate messages due to retransmission"
        );

        assert_sometimes!(
            self.duplicate_count == 0,
            "Receiver should sometimes see no duplicates (clean delivery)"
        );
    }

    /// Validate invariants at quiescence (after drain phase).
    pub fn validate_at_quiescence(&self) {
        let missing: Vec<_> = self
            .reliable_sent
            .difference(&self.reliable_received)
            .collect();

        assert_always!(
            missing.is_empty(),
            format!(
                "Not all reliable messages delivered at quiescence. Missing {} of {}: {:?}",
                missing.len(),
                self.reliable_sent.len(),
                if missing.len() > 10 {
                    &missing[..10]
                } else {
                    &missing[..]
                }
            )
        );

        assert_sometimes!(
            self.unreliable_received.len() < self.unreliable_sent.len(),
            "Some unreliable messages should sometimes be dropped"
        );

        assert_sometimes!(
            self.unreliable_received.len() == self.unreliable_sent.len(),
            "All unreliable messages should sometimes be delivered (no chaos)"
        );
    }

    /// Check if ordering is preserved for reliable messages.
    pub fn validate_ordering(&self) {
        let expected_order: Vec<u64> = self
            .reliable_sent_order
            .iter()
            .filter(|id| self.reliable_received.contains(id))
            .copied()
            .collect();

        if expected_order != self.reliable_received_order {
            for (i, (expected, actual)) in expected_order
                .iter()
                .zip(self.reliable_received_order.iter())
                .enumerate()
            {
                if expected != actual {
                    assert_always!(
                        false,
                        format!(
                            "Ordering violation at position {}: expected {}, got {}",
                            i, expected, actual
                        )
                    );
                    return;
                }
            }

            assert_always!(
                false,
                format!(
                    "Ordering length mismatch: expected {}, got {}",
                    expected_order.len(),
                    self.reliable_received_order.len()
                )
            );
        }
    }

    /// Get summary statistics for reporting.
    pub fn summary(&self) -> InvariantSummary {
        InvariantSummary {
            reliable_sent: self.reliable_sent.len(),
            reliable_received: self.reliable_received.len(),
            unreliable_sent: self.unreliable_sent.len(),
            unreliable_received: self.unreliable_received.len(),
            duplicates: self.duplicate_count,
        }
    }
}

/// Summary statistics from invariant tracking.
#[derive(Debug, Clone)]
pub struct InvariantSummary {
    /// Number of reliable messages sent.
    pub reliable_sent: usize,
    /// Number of reliable messages received.
    pub reliable_received: usize,
    /// Number of unreliable messages sent.
    pub unreliable_sent: usize,
    /// Number of unreliable messages received.
    pub unreliable_received: usize,
    /// Number of duplicate messages received.
    pub duplicates: u64,
}

impl std::fmt::Display for InvariantSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Reliable: {}/{} delivered, Unreliable: {}/{} delivered, Duplicates: {}",
            self.reliable_received,
            self.reliable_sent,
            self.unreliable_received,
            self.unreliable_sent,
            self.duplicates
        )
    }
}

/// Per-peer invariant tracking for multi-peer scenarios.
#[derive(Debug, Default)]
pub struct MultiPeerInvariants {
    /// Invariants per peer (keyed by sender_id).
    per_peer: HashMap<String, MessageInvariants>,
}

impl MultiPeerInvariants {
    /// Create a new multi-peer tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a message being sent.
    pub fn record_sent(&mut self, sender_id: &str, sequence_id: u64, reliable: bool) {
        self.per_peer
            .entry(sender_id.to_string())
            .or_default()
            .record_sent(sequence_id, reliable);
    }

    /// Record a message being received.
    pub fn record_received(&mut self, sender_id: &str, sequence_id: u64, sent_reliably: bool) {
        self.per_peer
            .entry(sender_id.to_string())
            .or_default()
            .record_received(sequence_id, sent_reliably);
    }

    /// Validate all invariants.
    pub fn validate_always(&self) {
        for (peer_id, invariants) in &self.per_peer {
            tracing::trace!("Validating invariants for peer {}", peer_id);
            invariants.validate_always();
        }
    }

    /// Validate at quiescence for all peers.
    pub fn validate_at_quiescence(&self) {
        for (peer_id, invariants) in &self.per_peer {
            tracing::debug!(
                "Validating quiescence for peer {}: {}",
                peer_id,
                invariants.summary()
            );
            invariants.validate_at_quiescence();
            invariants.validate_ordering();
        }
    }

    /// Get summary for all peers.
    pub fn summary(&self) -> HashMap<String, InvariantSummary> {
        self.per_peer
            .iter()
            .map(|(k, v)| (k.clone(), v.summary()))
            .collect()
    }
}
