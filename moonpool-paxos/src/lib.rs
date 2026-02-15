//! # Moonpool Paxos: Vertical Paxos II Consensus
//!
//! This crate implements the [Vertical Paxos II][vpii] consensus protocol for
//! the moonpool deterministic simulation framework. It provides primary-backup
//! replication with reconfiguration, designed for simulation testing with chaos
//! fault injection.
//!
//! [vpii]: https://www.microsoft.com/en-us/research/wp-content/uploads/2009/05/podc09v6.pdf
//!
//! ## Vertical Paxos II vs Raft: A Mental Model
//!
//! If you're familiar with Raft, here's how VP II concepts map:
//!
//! | VP II Concept | Raft Equivalent | Notes |
//! |---|---|---|
//! | **Ballot** | Term | Monotonically increasing epoch identifier |
//! | **Acceptor** | Voter/Follower | Stores votes, responds to leader |
//! | **Leader/Primary** | Leader | Drives consensus, serves client requests |
//! | **Configuration Master** | *(no equivalent)* | External entity that manages ballots/configs |
//! | **Phase 1 (Prepare)** | RequestVote | Leader learns about prior votes |
//! | **Phase 2 (Accept)** | AppendEntries | Leader replicates a value to acceptors |
//! | **Write quorum = ALL** | Majority quorum | VP II requires ALL acceptors, not just majority |
//! | **Read quorum = 1** | *(N/A)* | Any single acceptor has the full state |
//! | **Reconfiguration** | Membership change | VP II: master-driven, not log-based |
//!
//! ## Key Differences from Raft
//!
//! 1. **External Configuration Master**: Unlike Raft where the cluster itself
//!    manages membership, VP II uses an external "master" service that tracks
//!    configurations (ballot + leader + acceptor set). This simplifies the
//!    protocol but adds an external dependency.
//!
//! 2. **Write quorum = ALL acceptors**: VP II requires every acceptor to
//!    acknowledge a write before it's considered "chosen." This means any
//!    single acceptor is a valid "read quorum" (it has seen every chosen
//!    value). The tradeoff: higher write latency, but trivial state transfer
//!    during reconfiguration.
//!
//! 3. **Activation protocol**: A new configuration (ballot) starts "inactive."
//!    The new leader must complete Phase 1 (learning prior state) and report
//!    back to the master before the master "activates" the new ballot. Only
//!    then can the leader serve client requests. This prevents two leaders
//!    from being active simultaneously (no split-brain).
//!
//! 4. **Reconfiguration via master**: When the master detects a failed primary
//!    (via heartbeat timeout), it picks a new leader from the surviving
//!    acceptors. Since any single acceptor is a read quorum, the new leader
//!    already has all committed state locally — no state transfer needed.
//!
//! ## Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │                     PaxosClient                          │
//! │         Discovers primary, submits commands              │
//! └──────────────────────┬───────────────────────────────────┘
//!                        │ submit(command)
//!                        ▼
//! ┌──────────────────────────────────────────────────────────┐
//! │                  Leader (Primary)                         │
//! │   Phase 1: learn prior state from previous config        │
//! │   Phase 2: replicate to ALL acceptors                    │
//! │   Serves reads during lease                              │
//! └───────────┬──────────────────────────────┬───────────────┘
//!             │ Phase2a(bal, val)             │ heartbeat
//!             ▼                              ▼
//! ┌───────────────────┐          ┌───────────────────────────┐
//! │    Acceptors       │          │  Configuration Master     │
//! │  Store votes       │          │  Manages ballots          │
//! │  Respond to leader │          │  Activates new configs    │
//! └───────────────────┘          │  Detects failures         │
//!                                └───────────────────────────┘
//! ```
//!
//! ## Crate Organization
//!
//! | Module | Description |
//! |--------|-------------|
//! | [`types`] | Core types: `BallotNumber`, `LogSlot`, `Configuration`, `PaxosError` |
//! | [`storage`] | `PaxosStorage` trait and `InMemoryPaxosStorage` implementation |
//! | [`master`] | `ConfigurationMaster` trait and `InMemoryConfigMaster` |
//! | [`acceptor`] | Acceptor service: Phase 1a/2a handlers |
//! | [`leader`] | Leader service: Phase 1/2, client request path |

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod acceptor;
pub mod leader;
pub mod master;
pub mod storage;
pub mod types;

// Re-export key types at crate root for convenience
pub use master::{ConfigurationMaster, InMemoryConfigMaster};
pub use storage::{InMemoryPaxosStorage, PaxosStorage};
pub use types::{BallotNumber, Configuration, LogEntry, LogSlot, PaxosError};
