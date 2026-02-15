//! Core types for the Vertical Paxos II consensus protocol.
//!
//! This module defines the fundamental building blocks used throughout
//! the Paxos implementation:
//!
//! - [`BallotNumber`]: Monotonically increasing epoch identifier (like Raft's "term")
//! - [`LogSlot`]: Position in the replicated log (like Raft's "log index")
//! - [`Configuration`]: A ballot + its leader + acceptor set (like Raft's cluster membership)
//! - [`PaxosError`]: Error type for all Paxos operations
//! - [`LogEntry`]: A single entry in the replicated log

use moonpool_core::NetworkAddress;
use serde::{Deserialize, Serialize};

/// Ballot number — a monotonically increasing epoch identifier.
///
/// In Raft terms, this is equivalent to a **term number**. Each ballot
/// identifies a unique configuration (leader + acceptor set). A higher
/// ballot always takes precedence over a lower one.
///
/// # Invariants
///
/// - Ballot numbers are globally unique and monotonically increasing.
/// - The configuration master is the sole authority for issuing new ballots.
/// - An acceptor never votes in a ballot lower than its current `maxBallot`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct BallotNumber(pub u64);

impl BallotNumber {
    /// The initial ballot number (0), representing "no ballot seen yet."
    pub const ZERO: Self = Self(0);

    /// Create a new ballot number.
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// Get the next ballot number.
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for BallotNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ballot({})", self.0)
    }
}

/// Log slot — a position in the replicated log.
///
/// In Raft terms, this is equivalent to a **log index**. Each slot holds
/// at most one chosen value. Slots are filled sequentially by the leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LogSlot(pub u64);

impl LogSlot {
    /// The first valid log slot.
    pub const FIRST: Self = Self(0);

    /// Create a new log slot.
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// Get the next sequential slot.
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for LogSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "slot({})", self.0)
    }
}

/// A configuration in Vertical Paxos II.
///
/// In Raft terms, this is similar to a **cluster membership** — it defines
/// who the leader is and which nodes form the acceptor set. However, unlike
/// Raft where membership changes are committed through the log, VP II
/// configurations are managed by an external configuration master.
///
/// # VP II Specifics
///
/// - Each configuration is identified by a unique [`BallotNumber`].
/// - The `leader` is the primary that drives Phase 2 (replication).
/// - The `acceptors` are all nodes that must acknowledge writes (write quorum = ALL).
/// - A configuration starts **inactive** until the leader completes Phase 1
///   and the master activates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Configuration {
    /// The ballot number for this configuration (like Raft's term).
    pub ballot: BallotNumber,

    /// The leader (primary) of this configuration.
    ///
    /// This node drives Phase 2 and serves client requests once the
    /// configuration is activated.
    pub leader: NetworkAddress,

    /// The set of acceptors in this configuration.
    ///
    /// In VP II, the write quorum is ALL acceptors — every acceptor must
    /// acknowledge a Phase 2a message before a value is considered chosen.
    /// This means any single acceptor is a valid read quorum (it has seen
    /// every chosen value).
    pub acceptors: Vec<NetworkAddress>,
}

/// A single entry in the replicated log.
///
/// Each log entry records the ballot under which a value was accepted
/// and the serialized command bytes. In Raft terms, this is like a log
/// entry with its term number and command payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// The slot this entry occupies in the log.
    pub slot: LogSlot,

    /// The ballot under which this value was accepted.
    ///
    /// This is the ballot of the leader that proposed this value.
    /// In Raft terms, this is the "term" stored with each log entry.
    pub ballot: BallotNumber,

    /// The serialized command value.
    ///
    /// Stored as raw bytes to keep the consensus protocol generic
    /// over the application's command type.
    pub value: Vec<u8>,
}

/// Phase 1a message: "Prepare" request from leader to acceptors.
///
/// In Raft terms, this is like a **RequestVote RPC** — the leader is asking
/// acceptors whether they've already voted for a value in this slot.
///
/// The leader sends Phase1a to the *previous* configuration's acceptors
/// to learn about any values that may have been chosen before the
/// reconfiguration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase1aRequest {
    /// The ballot of the new configuration (the one being activated).
    pub ballot: BallotNumber,

    /// The log slot being queried.
    pub slot: LogSlot,
}

/// Phase 1b message: "Promise" response from acceptor to leader.
///
/// In Raft terms, this is like a **RequestVote response** — the acceptor
/// tells the leader about its most recent vote for this slot.
///
/// If the acceptor has voted, it returns the ballot and value of its
/// most recent vote. If it hasn't voted for this slot, both fields are `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase1bResponse {
    /// The ballot the acceptor is responding to.
    pub ballot: BallotNumber,

    /// The slot being queried.
    pub slot: LogSlot,

    /// The ballot of the acceptor's most recent vote for this slot, if any.
    ///
    /// `None` means the acceptor has never voted for this slot.
    pub voted_ballot: Option<BallotNumber>,

    /// The value the acceptor voted for, if any.
    ///
    /// Present only when `voted_ballot` is `Some`.
    pub voted_value: Option<Vec<u8>>,
}

/// Phase 2a message: "Accept" request from leader to acceptors.
///
/// In Raft terms, this is like an **AppendEntries RPC** — the leader is
/// asking acceptors to accept (store) a value for a specific log slot.
///
/// Key difference from Raft: VP II requires ALL acceptors to respond
/// (write quorum = all), not just a majority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase2aRequest {
    /// The leader's ballot number.
    pub ballot: BallotNumber,

    /// The log slot for this value.
    pub slot: LogSlot,

    /// The value to accept.
    pub value: Vec<u8>,
}

/// Phase 2b message: "Accepted" response from acceptor to leader.
///
/// In Raft terms, this is like an **AppendEntries response** — the acceptor
/// confirms it has stored the value (or rejects if it has moved to a
/// higher ballot).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase2bResponse {
    /// The ballot the acceptor is responding to.
    pub ballot: BallotNumber,

    /// The log slot that was (or wasn't) accepted.
    pub slot: LogSlot,

    /// Whether the acceptor accepted the value.
    ///
    /// `false` means the acceptor has moved to a higher ballot and
    /// rejected this request (stale ballot). In Raft terms, this is
    /// like receiving a response with a higher term.
    pub accepted: bool,
}

/// Client command submission request.
///
/// Sent by clients to the leader to replicate a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    /// The serialized command to replicate.
    pub command: Vec<u8>,
}

/// Client command submission response.
///
/// Returned by the leader after a command has been chosen (replicated
/// to all acceptors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResponse {
    /// The log slot where the command was placed.
    pub slot: LogSlot,

    /// Whether the command was successfully committed.
    ///
    /// `false` if the leader lost its ballot during replication
    /// (e.g., a reconfiguration happened).
    pub success: bool,
}

/// Result of the VFindSafe procedure for a single slot.
///
/// After Phase 1 (collecting 1b responses from the previous configuration),
/// the leader determines what value, if any, is "safe" to propose for each
/// slot. This is the core of VP II's reconfiguration safety guarantee.
///
/// In Raft terms, this is like the leader discovering uncommitted entries
/// from the previous term after winning an election — but VP II formalizes
/// this into a distinct protocol phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeValue {
    /// No acceptor in the previous configuration voted for this slot.
    ///
    /// The new leader is free to propose any value (including client commands).
    /// In Raft terms, this is like finding that no follower has an entry at
    /// this log index — the leader can write anything.
    Free,

    /// An acceptor voted for this value in a previous ballot.
    ///
    /// The new leader MUST re-propose this value before proposing new commands.
    /// This ensures that any value that *might* have been chosen in the previous
    /// configuration is preserved. In Raft terms, this is like the leader
    /// re-committing uncommitted entries from a previous term.
    Bound(Vec<u8>),
}

/// Errors that can occur during Paxos operations.
///
/// Maps to common failure modes in consensus protocols:
///
/// | Error | Raft Equivalent | When it happens |
/// |-------|-----------------|-----------------|
/// | `StaleBallot` | Stale term | Leader's ballot is outdated |
/// | `NotLeader` | Not leader | Node is not the active primary |
/// | `NotActivated` | *(N/A)* | Configuration hasn't been activated by master |
/// | `QuorumNotReached` | Replication failure | Not all acceptors responded |
/// | `Timeout` | Election/heartbeat timeout | Operation exceeded time limit |
/// | `Network` | RPC failure | Transport-level error |
/// | `Storage` | *(N/A)* | Persistent storage error |
#[derive(Debug, thiserror::Error)]
pub enum PaxosError {
    /// The ballot is stale — a higher ballot has been seen.
    ///
    /// In Raft terms: received a response with a higher term number.
    /// The leader should step down and stop serving requests.
    #[error("stale ballot: current {current}, seen {seen}")]
    StaleBallot {
        /// The leader's (stale) ballot.
        current: BallotNumber,
        /// The higher ballot that was observed.
        seen: BallotNumber,
    },

    /// This node is not the leader for the current configuration.
    ///
    /// Clients should redirect to the actual leader.
    #[error("not the leader")]
    NotLeader,

    /// The configuration has not been activated by the master yet.
    ///
    /// The leader must complete Phase 1 and receive activation from
    /// the master before serving requests.
    #[error("configuration not yet activated")]
    NotActivated,

    /// Write quorum was not reached — not all acceptors responded.
    ///
    /// In VP II, the write quorum is ALL acceptors, so even one
    /// missing response means the value was not chosen.
    #[error("quorum not reached: got {got} of {needed} responses")]
    QuorumNotReached {
        /// Number of successful responses received.
        got: usize,
        /// Number of responses needed (= total acceptors).
        needed: usize,
    },

    /// Operation timed out.
    #[error("operation timed out")]
    Timeout,

    /// Network/transport error.
    #[error("network error: {0}")]
    Network(String),

    /// Persistent storage error.
    #[error("storage error: {0}")]
    Storage(String),

    /// Serialization or deserialization error.
    #[error("codec error: {0}")]
    Codec(String),
}

impl From<moonpool_transport::RpcError> for PaxosError {
    fn from(err: moonpool_transport::RpcError) -> Self {
        PaxosError::Network(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_ballot_number_ordering() {
        let b0 = BallotNumber::ZERO;
        let b1 = BallotNumber::new(1);
        let b2 = BallotNumber::new(2);

        assert!(b0 < b1);
        assert!(b1 < b2);
        assert_eq!(b0.next(), b1);
    }

    #[test]
    fn test_ballot_number_display() {
        assert_eq!(BallotNumber::new(42).to_string(), "ballot(42)");
    }

    #[test]
    fn test_log_slot_ordering() {
        let s0 = LogSlot::FIRST;
        let s1 = LogSlot::new(1);

        assert!(s0 < s1);
        assert_eq!(s0.next(), s1);
    }

    #[test]
    fn test_log_slot_display() {
        assert_eq!(LogSlot::new(7).to_string(), "slot(7)");
    }

    #[test]
    fn test_configuration_serde_roundtrip() {
        let config = Configuration {
            ballot: BallotNumber::new(1),
            leader: NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5000),
            acceptors: vec![
                NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5000),
                NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 5000),
                NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 5000),
            ],
        };

        let json = serde_json::to_string(&config).expect("serialize");
        let decoded: Configuration = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(config, decoded);
    }

    #[test]
    fn test_log_entry_serde_roundtrip() {
        let entry = LogEntry {
            slot: LogSlot::new(5),
            ballot: BallotNumber::new(3),
            value: b"hello world".to_vec(),
        };

        let json = serde_json::to_string(&entry).expect("serialize");
        let decoded: LogEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry, decoded);
    }

    #[test]
    fn test_phase1a_serde() {
        let req = Phase1aRequest {
            ballot: BallotNumber::new(2),
            slot: LogSlot::new(0),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: Phase1aRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_phase1b_with_vote() {
        let resp = Phase1bResponse {
            ballot: BallotNumber::new(2),
            slot: LogSlot::new(0),
            voted_ballot: Some(BallotNumber::new(1)),
            voted_value: Some(b"prior value".to_vec()),
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let decoded: Phase1bResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(resp, decoded);
    }

    #[test]
    fn test_phase1b_no_vote() {
        let resp = Phase1bResponse {
            ballot: BallotNumber::new(2),
            slot: LogSlot::new(0),
            voted_ballot: None,
            voted_value: None,
        };
        assert!(resp.voted_ballot.is_none());
        assert!(resp.voted_value.is_none());
    }

    #[test]
    fn test_phase2a_serde() {
        let req = Phase2aRequest {
            ballot: BallotNumber::new(3),
            slot: LogSlot::new(1),
            value: b"command data".to_vec(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: Phase2aRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_phase2b_accepted() {
        let resp = Phase2bResponse {
            ballot: BallotNumber::new(3),
            slot: LogSlot::new(1),
            accepted: true,
        };
        assert!(resp.accepted);
    }

    #[test]
    fn test_phase2b_rejected() {
        let resp = Phase2bResponse {
            ballot: BallotNumber::new(3),
            slot: LogSlot::new(1),
            accepted: false,
        };
        assert!(!resp.accepted);
    }

    #[test]
    fn test_paxos_error_display() {
        let err = PaxosError::StaleBallot {
            current: BallotNumber::new(1),
            seen: BallotNumber::new(5),
        };
        assert!(err.to_string().contains("stale ballot"));

        let err = PaxosError::QuorumNotReached { got: 2, needed: 3 };
        assert!(err.to_string().contains("2 of 3"));
    }

    #[test]
    fn test_safe_value_variants() {
        let free = SafeValue::Free;
        assert!(matches!(free, SafeValue::Free));

        let bound = SafeValue::Bound(b"data".to_vec());
        assert!(matches!(bound, SafeValue::Bound(_)));
    }

    #[test]
    fn test_command_request_serde() {
        let req = CommandRequest {
            command: b"set x=42".to_vec(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: CommandRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_command_response_serde() {
        let resp = CommandResponse {
            slot: LogSlot::new(10),
            success: true,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        let decoded: CommandResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(resp, decoded);
    }
}
