//! Message tracking and invariant validation for simulation tests.
//!
//! Tracks sent/received messages and validates correctness properties.

#![allow(dead_code)] // Utility methods may not all be used in every test

use std::collections::HashSet;

use moonpool_sim::{Invariant, StateHandle, assert_always, assert_sometimes};

/// Tracks messages sent and received for invariant validation.
#[derive(Debug, Default, Clone)]
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

        // Unreliable can drop but never multiply (per-message)
        assert_always!(
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
        assert_sometimes!(
            self.duplicate_count > 0,
            "Should sometimes see duplicates due to retransmission"
        );

        // Also track clean delivery path
        assert_sometimes!(
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

        assert_always!(
            missing.is_empty(),
            format!(
                "Not all reliable messages delivered. Missing: {:?}",
                missing
            )
        );

        // Coverage assertions for unreliable behavior
        assert_sometimes!(
            self.unreliable_received.len() < self.unreliable_sent.len(),
            "Some unreliable should sometimes be dropped under chaos"
        );

        assert_sometimes!(
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

// ============================================================================
// RPC Invariants
// ============================================================================

/// Tracks RPC request-response pairs for invariant validation.
#[derive(Debug, Default, Clone)]
pub struct RpcInvariants {
    /// RPC requests sent (by request_id)
    pub requests_sent: HashSet<u64>,
    /// RPC responses received (by request_id)
    pub responses_received: HashSet<u64>,
    /// Broken promises detected (by request_id)
    pub broken_promises: HashSet<u64>,
    /// Connection failures count
    pub connection_failures: u64,
    /// Timeouts count
    pub timeouts: u64,
    /// Successful responses count
    pub successful_responses: u64,
}

impl RpcInvariants {
    /// Create a new empty RPC invariant tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an RPC request sent.
    pub fn record_request_sent(&mut self, request_id: u64) {
        self.requests_sent.insert(request_id);
    }

    /// Record an RPC response received (success).
    pub fn record_response_received(&mut self, request_id: u64) {
        self.responses_received.insert(request_id);
        self.successful_responses += 1;
    }

    /// Record a broken promise error received.
    pub fn record_broken_promise(&mut self, request_id: u64) {
        self.broken_promises.insert(request_id);
    }

    /// Record a connection failure.
    pub fn record_connection_failure(&mut self) {
        self.connection_failures += 1;
    }

    /// Record a timeout.
    pub fn record_timeout(&mut self) {
        self.timeouts += 1;
    }

    /// Validate invariants that should always hold.
    pub fn validate_always(&self) {
        // INV-1: No phantom responses - received must be subset of sent
        for request_id in &self.responses_received {
            assert_always!(
                self.requests_sent.contains(request_id),
                format!(
                    "Received RPC response {} that was never requested",
                    request_id
                )
            );
        }

        // INV-2: No phantom broken promises
        for request_id in &self.broken_promises {
            assert_always!(
                self.requests_sent.contains(request_id),
                format!(
                    "Received broken promise {} that was never requested",
                    request_id
                )
            );
        }

        // INV-3: At most one resolution per request (success XOR broken)
        for request_id in &self.responses_received {
            assert_always!(
                !self.broken_promises.contains(request_id),
                format!(
                    "Request {} has both success response and broken promise",
                    request_id
                )
            );
        }
    }

    /// Validate coverage assertions for error paths.
    pub fn validate_coverage(&self) {
        // Success path coverage
        assert_sometimes!(
            self.successful_responses > 0,
            "Should sometimes see successful RPC responses"
        );

        // Broken promise path coverage
        assert_sometimes!(
            !self.broken_promises.is_empty(),
            "Should sometimes see broken promises"
        );

        // Timeout path coverage
        assert_sometimes!(self.timeouts > 0, "Should sometimes see timeouts");
    }

    /// Get summary for logging.
    pub fn summary(&self) -> String {
        format!(
            "RPC: sent={}, success={}, broken={}, timeouts={}, conn_fail={}",
            self.requests_sent.len(),
            self.successful_responses,
            self.broken_promises.len(),
            self.timeouts,
            self.connection_failures
        )
    }
}

// ============================================================================
// Invariant trait wrappers for use with SimulationBuilder
// ============================================================================

/// Invariant wrapper that validates MessageInvariants from StateHandle.
pub struct MessageInvariantChecker;

impl Invariant for MessageInvariantChecker {
    fn name(&self) -> &str {
        "message_invariants"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(inv) = state.get::<MessageInvariants>("message_invariants") {
            inv.validate_always();
        }
    }
}

/// Invariant wrapper that validates RpcInvariants from StateHandle.
pub struct RpcInvariantChecker;

impl Invariant for RpcInvariantChecker {
    fn name(&self) -> &str {
        "rpc_invariants"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(inv) = state.get::<RpcInvariants>("rpc_invariants") {
            inv.validate_always();
        }
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

    // ========================================================================
    // RPC Invariants Tests
    // ========================================================================

    #[test]
    fn test_rpc_invariants_empty() {
        let inv = RpcInvariants::new();
        inv.validate_always(); // Should not panic
    }

    #[test]
    fn test_rpc_invariants_request_response() {
        let mut inv = RpcInvariants::new();

        inv.record_request_sent(1);
        inv.record_request_sent(2);
        inv.record_response_received(1);

        assert_eq!(inv.requests_sent.len(), 2);
        assert_eq!(inv.responses_received.len(), 1);
        assert_eq!(inv.successful_responses, 1);

        inv.validate_always(); // Should not panic
    }

    #[test]
    fn test_rpc_invariants_broken_promise() {
        let mut inv = RpcInvariants::new();

        inv.record_request_sent(1);
        inv.record_broken_promise(1);

        assert!(!inv.broken_promises.is_empty());
        inv.validate_always(); // Should not panic
    }

    #[test]
    #[should_panic(expected = "never requested")]
    fn test_rpc_phantom_response_detected() {
        let mut inv = RpcInvariants::new();

        // Receive response without sending request - should panic
        inv.record_response_received(999);
        inv.validate_always();
    }

    #[test]
    #[should_panic(expected = "never requested")]
    fn test_rpc_phantom_broken_promise_detected() {
        let mut inv = RpcInvariants::new();

        // Broken promise for never-sent request - should panic
        inv.record_broken_promise(999);
        inv.validate_always();
    }

    #[test]
    #[should_panic(expected = "both success response and broken promise")]
    fn test_rpc_double_resolution_detected() {
        let mut inv = RpcInvariants::new();

        inv.record_request_sent(1);
        inv.record_response_received(1);
        inv.record_broken_promise(1); // Should panic - already resolved

        inv.validate_always();
    }

    #[test]
    fn test_rpc_invariants_summary() {
        let mut inv = RpcInvariants::new();
        inv.record_request_sent(1);
        inv.record_request_sent(2);
        inv.record_response_received(1);
        inv.record_timeout();
        inv.record_connection_failure();

        let summary = inv.summary();
        assert!(summary.contains("sent=2"));
        assert!(summary.contains("success=1"));
        assert!(summary.contains("timeouts=1"));
        assert!(summary.contains("conn_fail=1"));
    }
}
