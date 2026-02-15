//! Catch-up protocol for lagging acceptors/backups.
//!
//! When a backup falls behind (e.g., after a network partition or restart),
//! it can request missing log entries from the primary via the catch-up RPC.
//! This is **not** part of the core Paxos protocol — it's a practical
//! optimization for recovery.
//!
//! ## Raft Comparison
//!
//! In Raft, a lagging follower catches up automatically via AppendEntries:
//! the leader detects the follower's `matchIndex` is behind and sends
//! the missing entries. In VP II, catch-up is a separate explicit request
//! from the backup to the primary.
//!
//! | Aspect | Raft | VP II |
//! |---|---|---|
//! | **Who initiates** | Leader (pushes missing entries) | Backup (pulls from primary) |
//! | **Mechanism** | AppendEntries with prevLogIndex | Explicit CatchUp RPC |
//! | **When** | Every heartbeat cycle | On-demand when backup detects gap |
//!
//! ## Protocol
//!
//! ```text
//! Backup                          Primary
//!   │                               │
//!   │  (detects gap in log)         │
//!   │                               │
//!   │── CatchUpRequest(from, to) ──>│
//!   │<── CatchUpResponse(entries) ──│
//!   │                               │
//!   │  (applies entries to log)     │
//! ```

use serde::{Deserialize, Serialize};

use crate::storage::PaxosStorage;
use crate::types::{LogEntry, LogSlot, PaxosError};

/// RPC interface ID for the catch-up service.
pub const CATCHUP_INTERFACE_ID: u64 = 0xCA7C_0001;

/// Method index for the catch-up RPC.
pub const CATCHUP_METHOD: u64 = 1;

/// Request to fetch missing log entries from the primary.
///
/// The backup sends this when it detects a gap in its log. For example,
/// after a network partition heals, the backup might have entries through
/// slot 10 but the primary is at slot 50 — the backup would request
/// entries from slot 11 to slot 50.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchUpRequest {
    /// First slot to fetch (inclusive).
    pub from_slot: LogSlot,

    /// Last slot to fetch (inclusive).
    pub to_slot: LogSlot,
}

/// Response containing the requested log entries.
///
/// The primary returns all entries it has in the requested range.
/// The response may contain fewer entries than requested if the primary
/// doesn't have all of them (e.g., if the range extends beyond the
/// primary's log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchUpResponse {
    /// The log entries in the requested range.
    ///
    /// Sorted by slot number. May be empty if no entries exist in the range.
    pub entries: Vec<LogEntry>,
}

/// Handle a catch-up request on the primary.
///
/// Reads the requested slot range from storage and returns the entries.
/// This is a simple read-only operation on the primary's log.
///
/// # Arguments
///
/// * `storage` - The primary's storage backend
/// * `request` - The catch-up request specifying the slot range
///
/// # Returns
///
/// A response containing all log entries in the requested range.
pub fn handle_catchup_request<S: PaxosStorage>(
    storage: &S,
    request: &CatchUpRequest,
) -> Result<CatchUpResponse, PaxosError> {
    let entries = storage.load_log_range(request.from_slot, request.to_slot)?;
    Ok(CatchUpResponse { entries })
}

/// Apply caught-up entries to a backup's storage.
///
/// Writes entries received from the primary into the backup's local storage.
/// Only writes entries that are newer (higher ballot) than what the backup
/// already has, to avoid overwriting more recent data.
///
/// # Arguments
///
/// * `storage` - The backup's storage backend
/// * `entries` - Entries received from the primary's catch-up response
///
/// # Returns
///
/// The number of entries that were actually written (some may be skipped
/// if the backup already has newer data).
pub fn apply_catchup_entries<S: PaxosStorage>(
    storage: &mut S,
    entries: &[LogEntry],
) -> Result<usize, PaxosError> {
    let mut applied = 0;

    for entry in entries {
        // Check if we already have a newer vote for this slot
        let existing = storage.load_vote(entry.slot)?;

        let should_write = match &existing {
            Some(existing_entry) => entry.ballot > existing_entry.ballot,
            None => true,
        };

        if should_write {
            storage.store_vote(entry.clone())?;
            applied += 1;
        }
    }

    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryPaxosStorage;
    use crate::types::BallotNumber;

    fn make_entry(slot: u64, ballot: u64, data: &str) -> LogEntry {
        LogEntry {
            slot: LogSlot::new(slot),
            ballot: BallotNumber::new(ballot),
            value: data.as_bytes().to_vec(),
        }
    }

    #[test]
    fn test_handle_catchup_request_empty() {
        let storage = InMemoryPaxosStorage::new();
        let request = CatchUpRequest {
            from_slot: LogSlot::new(0),
            to_slot: LogSlot::new(10),
        };

        let response = handle_catchup_request(&storage, &request).expect("handle");
        assert!(response.entries.is_empty());
    }

    #[test]
    fn test_handle_catchup_request_with_entries() {
        let mut storage = InMemoryPaxosStorage::new();

        for i in 0..5 {
            storage
                .store_vote(make_entry(i, 1, &format!("cmd-{}", i)))
                .expect("store");
        }

        let request = CatchUpRequest {
            from_slot: LogSlot::new(1),
            to_slot: LogSlot::new(3),
        };

        let response = handle_catchup_request(&storage, &request).expect("handle");
        assert_eq!(response.entries.len(), 3);
        assert_eq!(response.entries[0].slot, LogSlot::new(1));
        assert_eq!(response.entries[2].slot, LogSlot::new(3));
    }

    #[test]
    fn test_apply_catchup_entries_to_empty() {
        let mut storage = InMemoryPaxosStorage::new();

        let entries = vec![
            make_entry(0, 1, "cmd-0"),
            make_entry(1, 1, "cmd-1"),
            make_entry(2, 1, "cmd-2"),
        ];

        let applied = apply_catchup_entries(&mut storage, &entries).expect("apply");
        assert_eq!(applied, 3);

        // Verify entries were stored
        let loaded = storage
            .load_vote(LogSlot::new(1))
            .expect("load")
            .expect("should exist");
        assert_eq!(loaded.value, b"cmd-1");
    }

    #[test]
    fn test_apply_catchup_skips_older_entries() {
        let mut storage = InMemoryPaxosStorage::new();

        // Backup already has a newer entry at slot 1
        storage
            .store_vote(make_entry(1, 5, "newer"))
            .expect("store");

        let entries = vec![
            make_entry(0, 1, "cmd-0"),
            make_entry(1, 1, "cmd-1"), // older ballot — should be skipped
            make_entry(2, 1, "cmd-2"),
        ];

        let applied = apply_catchup_entries(&mut storage, &entries).expect("apply");
        assert_eq!(applied, 2); // only slots 0 and 2

        // Slot 1 should still have the newer entry
        let loaded = storage
            .load_vote(LogSlot::new(1))
            .expect("load")
            .expect("should exist");
        assert_eq!(loaded.value, b"newer");
        assert_eq!(loaded.ballot, BallotNumber::new(5));
    }

    #[test]
    fn test_apply_catchup_overwrites_older_entries() {
        let mut storage = InMemoryPaxosStorage::new();

        // Backup has an older entry at slot 1
        storage.store_vote(make_entry(1, 1, "old")).expect("store");

        let entries = vec![make_entry(1, 5, "new")]; // newer ballot

        let applied = apply_catchup_entries(&mut storage, &entries).expect("apply");
        assert_eq!(applied, 1);

        let loaded = storage
            .load_vote(LogSlot::new(1))
            .expect("load")
            .expect("should exist");
        assert_eq!(loaded.value, b"new");
        assert_eq!(loaded.ballot, BallotNumber::new(5));
    }

    #[test]
    fn test_catchup_request_serde() {
        let request = CatchUpRequest {
            from_slot: LogSlot::new(5),
            to_slot: LogSlot::new(20),
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let decoded: CatchUpRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(request, decoded);
    }

    #[test]
    fn test_catchup_response_serde() {
        let response = CatchUpResponse {
            entries: vec![make_entry(5, 1, "hello"), make_entry(6, 1, "world")],
        };
        let json = serde_json::to_string(&response).expect("serialize");
        let decoded: CatchUpResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(response, decoded);
    }
}
