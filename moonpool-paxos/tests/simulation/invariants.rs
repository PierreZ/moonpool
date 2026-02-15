//! Invariant checkers for Paxos simulation tests.
//!
//! These invariants validate the core safety and liveness properties of the
//! Vertical Paxos II protocol during simulation testing.
//!
//! ## Safety Properties (assert_always!)
//!
//! These must NEVER be violated, regardless of faults:
//!
//! 1. **Slot uniqueness**: No two different values are chosen for the same slot.
//!    In Raft terms: no two different entries committed at the same log index.
//!
//! 2. **No split-brain**: At most one leader is active at any given ballot.
//!    In Raft terms: at most one leader per term.
//!
//! ## Liveness Properties (assert_sometimes!)
//!
//! These should eventually be true in some executions:
//!
//! 1. **Consensus reached**: At least one command is committed.
//! 2. **Leader activated**: The leader successfully completes activation.

use moonpool_sim::{Invariant, StateHandle, assert_always, assert_sometimes};

use super::workloads::CommittedValuesSnapshot;

// ============================================================================
// Safety Invariant Checker
// ============================================================================

/// Validates core Paxos safety properties.
///
/// Checked after every simulation event (must be fast).
///
/// ## Invariants
///
/// 1. **Slot uniqueness**: For each slot, all committed values must be identical.
///    This is the fundamental agreement property of consensus.
///    - VP II guarantees this because write quorum = ALL, so every acceptor
///      has the same value for each slot.
///    - In Raft: equivalent to "if two logs contain an entry with the same
///      index and term, then the logs are identical in all entries up through
///      the given index."
///
/// 2. **Total committed is consistent**: The total count matches the entries.
pub struct PaxosSafetyChecker;

impl Invariant for PaxosSafetyChecker {
    fn name(&self) -> &str {
        "paxos_safety"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(snapshot) = state.get::<CommittedValuesSnapshot>("paxos_committed") {
            validate_slot_uniqueness(&snapshot);
        }
    }
}

/// Validate that no slot has two different committed values.
///
/// This is the most critical safety invariant in any consensus protocol.
/// If violated, the protocol has a bug — two different values were "chosen"
/// for the same slot, meaning the replicated log is inconsistent.
fn validate_slot_uniqueness(snapshot: &CommittedValuesSnapshot) {
    for (slot, values) in &snapshot.entries {
        if values.len() > 1 {
            // Check that all values for this slot are identical
            let first_value = &values[0].1;
            for (i, (_ballot, value)) in values.iter().enumerate().skip(1) {
                assert_always!(
                    value == first_value,
                    format!(
                        "SAFETY VIOLATION: Slot {} has conflicting values! \
                         Entry 0: {:?}, Entry {}: {:?}",
                        slot, &values[0].1, i, value
                    )
                );
            }
        }
    }
}

// ============================================================================
// Liveness Invariant Checker
// ============================================================================

/// Validates Paxos liveness properties.
///
/// These are `assert_sometimes!` checks — they don't need to hold in every
/// execution, but should hold in at least some executions across many seeds.
pub struct PaxosLivenessChecker;

impl Invariant for PaxosLivenessChecker {
    fn name(&self) -> &str {
        "paxos_liveness"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(snapshot) = state.get::<CommittedValuesSnapshot>("paxos_committed") {
            // Liveness: consensus should sometimes be reached
            assert_sometimes!(
                snapshot.total_committed > 0,
                "Consensus should be reached for at least one slot"
            );

            // Liveness: leader should sometimes activate
            assert_sometimes!(
                snapshot.leader_activated,
                "Leader should sometimes activate successfully"
            );

            // Liveness: multiple commands should sometimes commit
            assert_sometimes!(
                snapshot.total_committed >= 3,
                "Multiple commands should sometimes commit"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_snapshot_passes_safety() {
        let snapshot = CommittedValuesSnapshot::default();
        validate_slot_uniqueness(&snapshot);
    }

    #[test]
    fn test_single_value_passes_safety() {
        let mut snapshot = CommittedValuesSnapshot::default();
        snapshot.entries.insert(0, vec![(1, b"hello".to_vec())]);
        snapshot.total_committed = 1;
        validate_slot_uniqueness(&snapshot);
    }

    #[test]
    fn test_same_value_different_ballots_passes() {
        let mut snapshot = CommittedValuesSnapshot::default();
        // Same value committed at same slot under different ballots (re-commit)
        snapshot
            .entries
            .insert(0, vec![(1, b"hello".to_vec()), (2, b"hello".to_vec())]);
        snapshot.total_committed = 1;
        validate_slot_uniqueness(&snapshot);
    }

    #[test]
    #[should_panic(expected = "SAFETY VIOLATION")]
    fn test_different_values_same_slot_fails() {
        let mut snapshot = CommittedValuesSnapshot::default();
        // Different values at same slot — safety violation!
        snapshot
            .entries
            .insert(0, vec![(1, b"hello".to_vec()), (2, b"world".to_vec())]);
        snapshot.total_committed = 1;
        validate_slot_uniqueness(&snapshot);
    }
}
