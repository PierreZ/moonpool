//! Paxos storage trait and in-memory implementation.
//!
//! The [`PaxosStorage`] trait abstracts persistent state for both acceptors
//! and leaders. In a real deployment, this would be backed by durable storage
//! (e.g., WAL or LSM tree). For simulation testing, [`InMemoryPaxosStorage`]
//! provides a simple in-memory implementation.
//!
//! ## What gets stored?
//!
//! Each acceptor needs to persist:
//! - **maxBallot**: The highest ballot it has promised not to vote below.
//!   In Raft terms, this is like the `currentTerm` persisted on each server.
//! - **votes**: For each log slot, the ballot and value of the most recent
//!   vote. In Raft terms, this is like the `votedFor` + log entries.
//!
//! The leader also uses storage for its log of committed entries.

use std::collections::BTreeMap;

use crate::types::{BallotNumber, LogEntry, LogSlot, PaxosError};

/// Trait for persistent Paxos state storage.
///
/// This trait abstracts the durable state that Paxos acceptors and leaders
/// need to persist across crashes. Implementations must ensure that writes
/// are durable before returning (in a real system, this means fsync).
///
/// ## Raft comparison
///
/// In Raft, each server persists `currentTerm`, `votedFor`, and `log[]`.
/// The Paxos equivalents are:
/// - `max_ballot` ≈ `currentTerm` (highest term/ballot seen)
/// - `vote` for a slot ≈ `votedFor` + the log entry at that index
/// - `log entries` ≈ `log[]` (the committed log)
pub trait PaxosStorage {
    /// Load the highest ballot this node has promised.
    ///
    /// Returns `BallotNumber::ZERO` if no ballot has been stored yet.
    /// In Raft terms: loading `currentTerm` from disk.
    fn load_max_ballot(&self) -> Result<BallotNumber, PaxosError>;

    /// Store the highest ballot this node has promised.
    ///
    /// Must be durable before returning. Called when an acceptor receives
    /// a Phase 1a or Phase 2a message with a ballot >= its current maxBallot.
    /// In Raft terms: persisting an updated `currentTerm`.
    fn store_max_ballot(&mut self, ballot: BallotNumber) -> Result<(), PaxosError>;

    /// Load the vote (accepted value) for a specific log slot.
    ///
    /// Returns `None` if the acceptor has never voted for this slot.
    /// In Raft terms: reading the log entry at a specific index.
    fn load_vote(&self, slot: LogSlot) -> Result<Option<LogEntry>, PaxosError>;

    /// Store a vote (accepted value) for a specific log slot.
    ///
    /// Must be durable before returning. Called when an acceptor accepts
    /// a Phase 2a message. The entry records which ballot proposed the value
    /// and the value itself.
    /// In Raft terms: persisting a log entry with its term.
    fn store_vote(&mut self, entry: LogEntry) -> Result<(), PaxosError>;

    /// Load all log entries in a slot range (inclusive).
    ///
    /// Used during catch-up: a lagging backup requests entries from
    /// `from_slot` to `to_slot` from the primary.
    fn load_log_range(
        &self,
        from_slot: LogSlot,
        to_slot: LogSlot,
    ) -> Result<Vec<LogEntry>, PaxosError>;

    /// Get the highest slot that has a stored vote.
    ///
    /// Returns `None` if no votes have been stored (empty log).
    /// Useful for determining the next slot to use for new commands.
    fn highest_slot(&self) -> Result<Option<LogSlot>, PaxosError>;
}

/// In-memory implementation of [`PaxosStorage`].
///
/// Suitable for simulation testing. All state is lost on "crash" (drop),
/// which is intentional for testing crash recovery scenarios.
#[derive(Debug, Default)]
pub struct InMemoryPaxosStorage {
    /// The highest ballot this node has promised.
    max_ballot: BallotNumber,

    /// Votes indexed by log slot.
    ///
    /// Uses a `BTreeMap` for ordered iteration (useful for `load_log_range`
    /// and `highest_slot`).
    votes: BTreeMap<LogSlot, LogEntry>,
}

impl InMemoryPaxosStorage {
    /// Create a new empty in-memory storage.
    pub fn new() -> Self {
        Self::default()
    }
}

impl PaxosStorage for InMemoryPaxosStorage {
    fn load_max_ballot(&self) -> Result<BallotNumber, PaxosError> {
        Ok(self.max_ballot)
    }

    fn store_max_ballot(&mut self, ballot: BallotNumber) -> Result<(), PaxosError> {
        self.max_ballot = ballot;
        Ok(())
    }

    fn load_vote(&self, slot: LogSlot) -> Result<Option<LogEntry>, PaxosError> {
        Ok(self.votes.get(&slot).cloned())
    }

    fn store_vote(&mut self, entry: LogEntry) -> Result<(), PaxosError> {
        self.votes.insert(entry.slot, entry);
        Ok(())
    }

    fn load_log_range(
        &self,
        from_slot: LogSlot,
        to_slot: LogSlot,
    ) -> Result<Vec<LogEntry>, PaxosError> {
        Ok(self
            .votes
            .range(from_slot..=to_slot)
            .map(|(_, entry)| entry.clone())
            .collect())
    }

    fn highest_slot(&self) -> Result<Option<LogSlot>, PaxosError> {
        Ok(self.votes.keys().next_back().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_state() {
        let storage = InMemoryPaxosStorage::new();

        assert_eq!(storage.load_max_ballot().expect("load"), BallotNumber::ZERO);
        assert!(storage.highest_slot().expect("highest").is_none());
        assert!(storage.load_vote(LogSlot::FIRST).expect("load").is_none());
    }

    #[test]
    fn test_store_and_load_max_ballot() {
        let mut storage = InMemoryPaxosStorage::new();

        storage
            .store_max_ballot(BallotNumber::new(5))
            .expect("store");
        assert_eq!(
            storage.load_max_ballot().expect("load"),
            BallotNumber::new(5)
        );

        // Storing a higher ballot replaces the old one
        storage
            .store_max_ballot(BallotNumber::new(10))
            .expect("store");
        assert_eq!(
            storage.load_max_ballot().expect("load"),
            BallotNumber::new(10)
        );
    }

    #[test]
    fn test_store_and_load_vote() {
        let mut storage = InMemoryPaxosStorage::new();

        let entry = LogEntry {
            slot: LogSlot::new(0),
            ballot: BallotNumber::new(1),
            value: b"hello".to_vec(),
        };

        storage.store_vote(entry.clone()).expect("store");

        let loaded = storage
            .load_vote(LogSlot::new(0))
            .expect("load")
            .expect("should exist");
        assert_eq!(loaded, entry);

        // Non-existent slot returns None
        assert!(storage.load_vote(LogSlot::new(1)).expect("load").is_none());
    }

    #[test]
    fn test_store_vote_overwrites() {
        let mut storage = InMemoryPaxosStorage::new();

        let entry1 = LogEntry {
            slot: LogSlot::new(0),
            ballot: BallotNumber::new(1),
            value: b"first".to_vec(),
        };
        let entry2 = LogEntry {
            slot: LogSlot::new(0),
            ballot: BallotNumber::new(2),
            value: b"second".to_vec(),
        };

        storage.store_vote(entry1).expect("store");
        storage.store_vote(entry2.clone()).expect("store");

        let loaded = storage
            .load_vote(LogSlot::new(0))
            .expect("load")
            .expect("should exist");
        assert_eq!(loaded, entry2);
    }

    #[test]
    fn test_highest_slot() {
        let mut storage = InMemoryPaxosStorage::new();

        assert!(storage.highest_slot().expect("highest").is_none());

        storage
            .store_vote(LogEntry {
                slot: LogSlot::new(3),
                ballot: BallotNumber::new(1),
                value: b"a".to_vec(),
            })
            .expect("store");

        assert_eq!(
            storage.highest_slot().expect("highest"),
            Some(LogSlot::new(3))
        );

        storage
            .store_vote(LogEntry {
                slot: LogSlot::new(7),
                ballot: BallotNumber::new(1),
                value: b"b".to_vec(),
            })
            .expect("store");

        assert_eq!(
            storage.highest_slot().expect("highest"),
            Some(LogSlot::new(7))
        );
    }

    #[test]
    fn test_load_log_range() {
        let mut storage = InMemoryPaxosStorage::new();

        for i in 0..5 {
            storage
                .store_vote(LogEntry {
                    slot: LogSlot::new(i),
                    ballot: BallotNumber::new(1),
                    value: format!("entry-{}", i).into_bytes(),
                })
                .expect("store");
        }

        // Load slots 1..=3
        let entries = storage
            .load_log_range(LogSlot::new(1), LogSlot::new(3))
            .expect("load");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].slot, LogSlot::new(1));
        assert_eq!(entries[1].slot, LogSlot::new(2));
        assert_eq!(entries[2].slot, LogSlot::new(3));

        // Empty range
        let entries = storage
            .load_log_range(LogSlot::new(10), LogSlot::new(20))
            .expect("load");
        assert!(entries.is_empty());
    }
}
