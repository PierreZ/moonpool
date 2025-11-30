//! Message tracking and invariant validation for simulation tests.
//!
//! Tracks sent/received messages and validates correctness properties.

#![allow(dead_code)] // Utility methods may not all be used in every test

use std::collections::HashSet;

use moonpool_foundation::{always_assert, sometimes_assert};

/// Tracks messages sent and received for invariant validation.
#[derive(Debug, Default)]
pub struct MessageInvariants {
    /// Reliable messages sent (by seq_id)
    pub reliable_sent: HashSet<u64>,
    /// Reliable messages received (by seq_id)
    pub reliable_received: HashSet<u64>,
    /// Unreliable messages sent (by seq_id)
    pub unreliable_sent: HashSet<u64>,
    /// Unreliable messages received (by seq_id)
    pub unreliable_received: HashSet<u64>,

    /// Count of duplicate messages received
    pub duplicate_count: u64,
    /// Count of messages dropped due to deserialization errors
    pub messages_dropped: u64,
}

impl MessageInvariants {
    /// Create a new empty invariant tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a message sent.
    pub fn record_sent(&mut self, seq_id: u64, reliable: bool) {
        if reliable {
            self.reliable_sent.insert(seq_id);
        } else {
            self.unreliable_sent.insert(seq_id);
        }
    }

    /// Record a message received. Returns true if it was a duplicate.
    pub fn record_received(&mut self, seq_id: u64, reliable: bool) -> bool {
        let is_dup = if reliable {
            self.reliable_received.contains(&seq_id)
        } else {
            self.unreliable_received.contains(&seq_id)
        };

        if is_dup {
            self.duplicate_count += 1;
        }

        if reliable {
            self.reliable_received.insert(seq_id);
        } else {
            self.unreliable_received.insert(seq_id);
        }

        is_dup
    }

    /// Record a message dropped due to deserialization error.
    pub fn record_dropped(&mut self) {
        self.messages_dropped += 1;
    }

    /// Validate invariants that should always hold.
    ///
    /// Called continuously during test execution.
    pub fn validate_always(&self) {
        // No phantom messages - received must be subset of sent
        for seq_id in &self.reliable_received {
            always_assert!(
                no_phantom_reliable,
                self.reliable_sent.contains(seq_id),
                format!("Received reliable message {} that was never sent", seq_id)
            );
        }

        for seq_id in &self.unreliable_received {
            always_assert!(
                no_phantom_unreliable,
                self.unreliable_sent.contains(seq_id),
                format!("Received unreliable message {} that was never sent", seq_id)
            );
        }

        // Unreliable can drop but never multiply (per-message)
        always_assert!(
            unreliable_conservation,
            self.unreliable_received.len() <= self.unreliable_sent.len(),
            format!(
                "More unique unreliable received ({}) than sent ({})",
                self.unreliable_received.len(),
                self.unreliable_sent.len()
            )
        );
    }

    /// Validate receiver-side invariants with coverage assertions.
    pub fn validate_receiver_always(&self) {
        // Track duplicate occurrence under chaos (expected with retransmission)
        sometimes_assert!(
            receiver_sees_duplicates,
            self.duplicate_count > 0,
            "Should sometimes see duplicates due to retransmission"
        );

        // Also track clean delivery path
        sometimes_assert!(
            receiver_no_duplicates,
            self.duplicate_count == 0,
            "Should sometimes see no duplicates (clean delivery)"
        );
    }

    /// Validate at quiescence (after drain phase).
    ///
    /// Checks that all reliable messages were eventually delivered.
    pub fn validate_at_quiescence(&self) {
        // All reliable messages should be delivered
        let missing: Vec<_> = self
            .reliable_sent
            .difference(&self.reliable_received)
            .collect();

        always_assert!(
            all_reliable_delivered,
            missing.is_empty(),
            format!(
                "Not all reliable messages delivered. Missing: {:?}",
                missing
            )
        );

        // Coverage assertions for unreliable behavior
        sometimes_assert!(
            some_unreliable_dropped,
            self.unreliable_received.len() < self.unreliable_sent.len(),
            "Some unreliable should sometimes be dropped under chaos"
        );

        sometimes_assert!(
            all_unreliable_delivered,
            self.unreliable_received.len() == self.unreliable_sent.len(),
            "All unreliable should sometimes be delivered (no chaos)"
        );
    }

    /// Get total messages sent (reliable + unreliable).
    pub fn total_sent(&self) -> usize {
        self.reliable_sent.len() + self.unreliable_sent.len()
    }

    /// Get total messages received (reliable + unreliable).
    pub fn total_received(&self) -> usize {
        self.reliable_received.len() + self.unreliable_received.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_invariants() {
        let inv = MessageInvariants::new();
        inv.validate_always(); // Should not panic
    }

    #[test]
    fn test_record_sent_received() {
        let mut inv = MessageInvariants::new();

        inv.record_sent(1, true);
        inv.record_sent(2, false);

        assert_eq!(inv.reliable_sent.len(), 1);
        assert_eq!(inv.unreliable_sent.len(), 1);

        let dup = inv.record_received(1, true);
        assert!(!dup);

        let dup = inv.record_received(1, true);
        assert!(dup);
        assert_eq!(inv.duplicate_count, 1);
    }

    #[test]
    fn test_validate_always_passes() {
        let mut inv = MessageInvariants::new();

        inv.record_sent(1, true);
        inv.record_sent(2, true);
        inv.record_received(1, true);

        inv.validate_always(); // Should not panic
    }

    #[test]
    #[should_panic(expected = "never sent")]
    fn test_phantom_message_detected() {
        let mut inv = MessageInvariants::new();

        // Receive without sending - should panic
        inv.record_received(999, true);
        inv.validate_always();
    }
}
