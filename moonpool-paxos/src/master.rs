//! Configuration master trait and in-memory implementation.
//!
//! In Vertical Paxos II, the **configuration master** is an external service
//! that manages ballot numbers and configuration lifecycle. Unlike Raft, where
//! the cluster itself handles leader election and membership changes, VP II
//! delegates this to a separate entity.
//!
//! ## The Master's Role
//!
//! The master is responsible for:
//!
//! 1. **Issuing new ballots** (`new_ballot`): When a reconfiguration is needed
//!    (e.g., primary failure), the master picks a new leader and issues a new
//!    ballot number with the new configuration.
//!
//! 2. **Tracking activation** (`report_complete` / `wait_activated`): A new
//!    configuration starts **inactive**. The new leader must complete Phase 1
//!    (learning prior state from the previous configuration) and then report
//!    back to the master. Only after the master confirms activation can the
//!    leader begin serving client requests. This prevents split-brain.
//!
//! 3. **Providing current config** (`current_config`): Clients and nodes can
//!    query the master to discover the current active configuration (who is
//!    the leader, which nodes are acceptors).
//!
//! ## VP II Activation Protocol (Figure 5 in the paper)
//!
//! ```text
//! Master                    New Leader
//!   │                          │
//!   │── new_ballot(bal) ──────>│  (1) Master issues new ballot
//!   │                          │
//!   │                          │  (2) Leader does Phase 1 with prev config
//!   │                          │      (sends 1a, collects 1b responses)
//!   │                          │
//!   │                          │  (3) If Phase 1 found bound values,
//!   │                          │      leader does Phase 2 to re-commit them
//!   │                          │
//!   │<── report_complete(bal)──│  (4) Leader tells master it's ready
//!   │                          │
//!   │── activated(bal) ───────>│  (5) Master activates, leader can serve
//!   │                          │
//! ```
//!
//! ## Safety Guarantee
//!
//! The master ensures that at most one configuration is active at a time.
//! It will only activate a new ballot after the previous one has been
//! superseded (its leader stopped or the new leader completed Phase 1
//! which ensures continuity of any chosen values).

use crate::types::{BallotNumber, Configuration, PaxosError};

/// Trait for the VP II configuration master.
///
/// The configuration master is the external authority that manages
/// ballot numbers and configuration activation. In a production system,
/// this might be implemented as a separate highly-available service
/// (e.g., backed by ZooKeeper, etcd, or another Paxos group).
///
/// ## Raft comparison
///
/// Raft has no direct equivalent. The closest analogy is:
/// - `new_ballot` ≈ a candidate starting an election with a new term
/// - `report_complete` ≈ a candidate winning the election
/// - `wait_activated` ≈ the new leader knowing it can serve
/// - `current_config` ≈ the cluster knowing who the current leader is
///
/// But in Raft, all of this is distributed within the cluster itself.
/// VP II externalizes it to a master for simplicity.
pub trait ConfigurationMaster {
    /// Issue a new ballot for a configuration change.
    ///
    /// The master creates a new (higher) ballot number and associates it
    /// with the given leader and acceptor set. The new configuration starts
    /// in an **inactive** state.
    ///
    /// # Arguments
    ///
    /// * `config` - The new configuration to install (ballot must be higher
    ///   than any previously issued ballot).
    ///
    /// # Errors
    ///
    /// Returns `PaxosError::StaleBallot` if the ballot is not higher than
    /// the current one.
    fn new_ballot(&mut self, config: Configuration) -> Result<(), PaxosError>;

    /// Report that Phase 1 (and any necessary Phase 2 re-commits) are complete.
    ///
    /// The leader calls this after successfully completing the VFindSafe
    /// procedure and re-committing any bound values. The master then
    /// activates the configuration.
    ///
    /// # Arguments
    ///
    /// * `ballot` - The ballot that completed Phase 1.
    /// * `prev_ballot` - The previous ballot that was active (for verification).
    ///
    /// # Errors
    ///
    /// Returns `PaxosError::StaleBallot` if `prev_ballot` doesn't match
    /// the master's record of the previously active ballot.
    fn report_complete(
        &mut self,
        ballot: BallotNumber,
        prev_ballot: BallotNumber,
    ) -> Result<(), PaxosError>;

    /// Check whether a ballot has been activated.
    ///
    /// Returns `true` if the master has activated this ballot (i.e., the
    /// leader reported Phase 1 completion and the master confirmed).
    /// The leader polls this to know when it can start serving clients.
    fn is_activated(&self, ballot: BallotNumber) -> Result<bool, PaxosError>;

    /// Get the current active configuration.
    ///
    /// Returns the most recently activated configuration, or `None` if
    /// no configuration has been activated yet.
    ///
    /// Used by clients to discover the current leader and by nodes to
    /// verify their role.
    fn current_config(&self) -> Result<Option<Configuration>, PaxosError>;

    /// Get the previous configuration's ballot.
    ///
    /// Used by the leader when starting Phase 1 to know which configuration's
    /// acceptors to query.
    fn prev_ballot(&self, ballot: BallotNumber) -> Result<Option<BallotNumber>, PaxosError>;
}

/// In-memory implementation of the configuration master.
///
/// Implements the strict VP II activation protocol:
///
/// 1. `new_ballot(config)` → stores config as pending (inactive)
/// 2. `report_complete(bal, prev_bal)` → if prev_bal matches, activates bal
/// 3. `is_activated(bal)` → returns whether bal is active
/// 4. `current_config()` → returns the active config
///
/// ## Safety invariant
///
/// At most one configuration is active at any time. A new configuration
/// can only be activated after the leader proves it has learned all state
/// from the previous configuration (via Phase 1).
#[derive(Debug)]
pub struct InMemoryConfigMaster {
    /// The currently active configuration, if any.
    active_config: Option<Configuration>,

    /// The pending (not yet activated) configuration, if any.
    ///
    /// Set by `new_ballot`, cleared when activated by `report_complete`.
    pending_config: Option<Configuration>,

    /// The highest ballot ever issued.
    ///
    /// Used to enforce monotonically increasing ballots.
    highest_ballot: BallotNumber,
}

impl InMemoryConfigMaster {
    /// Create a new configuration master with no active configuration.
    pub fn new() -> Self {
        Self {
            active_config: None,
            pending_config: None,
            highest_ballot: BallotNumber::ZERO,
        }
    }

    /// Bootstrap the master with an initial configuration.
    ///
    /// The initial configuration is immediately activated (no Phase 1 needed
    /// because there is no previous configuration to learn from).
    ///
    /// This is how the system starts: the first configuration is special
    /// because it has no predecessor.
    pub fn bootstrap(&mut self, config: Configuration) -> Result<(), PaxosError> {
        if config.ballot <= self.highest_ballot {
            return Err(PaxosError::StaleBallot {
                current: self.highest_ballot,
                seen: config.ballot,
            });
        }
        self.highest_ballot = config.ballot;
        self.active_config = Some(config);
        self.pending_config = None;
        Ok(())
    }
}

impl Default for InMemoryConfigMaster {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigurationMaster for InMemoryConfigMaster {
    fn new_ballot(&mut self, config: Configuration) -> Result<(), PaxosError> {
        // Ballot must be strictly higher than anything we've seen
        if config.ballot <= self.highest_ballot {
            return Err(PaxosError::StaleBallot {
                current: self.highest_ballot,
                seen: config.ballot,
            });
        }

        self.highest_ballot = config.ballot;
        self.pending_config = Some(config);
        Ok(())
    }

    fn report_complete(
        &mut self,
        ballot: BallotNumber,
        prev_ballot: BallotNumber,
    ) -> Result<(), PaxosError> {
        // Verify the pending config matches
        let pending = match &self.pending_config {
            Some(c) if c.ballot == ballot => c.clone(),
            _ => {
                return Err(PaxosError::StaleBallot {
                    current: self.highest_ballot,
                    seen: ballot,
                });
            }
        };

        // Verify prev_ballot matches the currently active config
        let active_ballot = self
            .active_config
            .as_ref()
            .map(|c| c.ballot)
            .unwrap_or(BallotNumber::ZERO);

        if prev_ballot != active_ballot {
            return Err(PaxosError::StaleBallot {
                current: active_ballot,
                seen: prev_ballot,
            });
        }

        // Activate: move pending → active
        self.active_config = Some(pending);
        self.pending_config = None;
        Ok(())
    }

    fn is_activated(&self, ballot: BallotNumber) -> Result<bool, PaxosError> {
        Ok(self
            .active_config
            .as_ref()
            .map(|c| c.ballot == ballot)
            .unwrap_or(false))
    }

    fn current_config(&self) -> Result<Option<Configuration>, PaxosError> {
        Ok(self.active_config.clone())
    }

    fn prev_ballot(&self, ballot: BallotNumber) -> Result<Option<BallotNumber>, PaxosError> {
        // If there's a pending config with this ballot, the previous is the active config's ballot
        if let Some(pending) = &self.pending_config
            && pending.ballot == ballot
        {
            return Ok(self.active_config.as_ref().map(|c| c.ballot));
        }
        // If this IS the active config, there's no "previous" in our records
        // (we don't keep a full history)
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BallotNumber;
    use moonpool_core::NetworkAddress;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_addr(port: u16) -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port)
    }

    fn make_config(ballot: u64, leader_port: u16) -> Configuration {
        Configuration {
            ballot: BallotNumber::new(ballot),
            leader: make_addr(leader_port),
            acceptors: vec![make_addr(5001), make_addr(5002), make_addr(5003)],
        }
    }

    #[test]
    fn test_new_master_has_no_config() {
        let master = InMemoryConfigMaster::new();
        assert!(master.current_config().expect("config").is_none());
        assert!(!master.is_activated(BallotNumber::new(1)).expect("check"));
    }

    #[test]
    fn test_bootstrap_activates_initial_config() {
        let mut master = InMemoryConfigMaster::new();
        let config = make_config(1, 5001);

        master.bootstrap(config.clone()).expect("bootstrap");

        let active = master
            .current_config()
            .expect("config")
            .expect("should exist");
        assert_eq!(active, config);
        assert!(master.is_activated(BallotNumber::new(1)).expect("check"));
    }

    #[test]
    fn test_bootstrap_rejects_stale_ballot() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(5, 5001)).expect("bootstrap");

        // Cannot bootstrap with a lower ballot
        let result = master.bootstrap(make_config(3, 5002));
        assert!(result.is_err());
    }

    #[test]
    fn test_new_ballot_creates_pending_config() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(1, 5001)).expect("bootstrap");

        // Issue new ballot — should be pending, not active
        let new_config = make_config(2, 5002);
        master.new_ballot(new_config).expect("new_ballot");

        // Active config should still be ballot 1
        let active = master
            .current_config()
            .expect("config")
            .expect("should exist");
        assert_eq!(active.ballot, BallotNumber::new(1));

        // Ballot 2 should NOT be activated yet
        assert!(!master.is_activated(BallotNumber::new(2)).expect("check"));
    }

    #[test]
    fn test_new_ballot_rejects_stale() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(5, 5001)).expect("bootstrap");

        let result = master.new_ballot(make_config(3, 5002));
        assert!(result.is_err());
    }

    #[test]
    fn test_report_complete_activates_config() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(1, 5001)).expect("bootstrap");

        // Issue and complete ballot 2
        master.new_ballot(make_config(2, 5002)).expect("new_ballot");
        master
            .report_complete(BallotNumber::new(2), BallotNumber::new(1))
            .expect("complete");

        // Ballot 2 should now be active
        let active = master
            .current_config()
            .expect("config")
            .expect("should exist");
        assert_eq!(active.ballot, BallotNumber::new(2));
        assert!(master.is_activated(BallotNumber::new(2)).expect("check"));

        // Ballot 1 should no longer be active
        assert!(!master.is_activated(BallotNumber::new(1)).expect("check"));
    }

    #[test]
    fn test_report_complete_wrong_prev_ballot() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(1, 5001)).expect("bootstrap");
        master.new_ballot(make_config(2, 5002)).expect("new_ballot");

        // Wrong prev_ballot (claiming ballot 5 was active, but it was ballot 1)
        let result = master.report_complete(BallotNumber::new(2), BallotNumber::new(5));
        assert!(result.is_err());
    }

    #[test]
    fn test_report_complete_wrong_ballot() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(1, 5001)).expect("bootstrap");
        master.new_ballot(make_config(2, 5002)).expect("new_ballot");

        // Completing a ballot that isn't pending
        let result = master.report_complete(BallotNumber::new(99), BallotNumber::new(1));
        assert!(result.is_err());
    }

    #[test]
    fn test_prev_ballot_for_pending() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(1, 5001)).expect("bootstrap");
        master.new_ballot(make_config(2, 5002)).expect("new_ballot");

        let prev = master
            .prev_ballot(BallotNumber::new(2))
            .expect("prev")
            .expect("should exist");
        assert_eq!(prev, BallotNumber::new(1));
    }

    #[test]
    fn test_full_reconfiguration_cycle() {
        let mut master = InMemoryConfigMaster::new();

        // Bootstrap initial config
        master.bootstrap(make_config(1, 5001)).expect("bootstrap");
        assert!(master.is_activated(BallotNumber::new(1)).expect("check"));

        // Reconfigure: ballot 2
        master.new_ballot(make_config(2, 5002)).expect("new_ballot");
        assert!(!master.is_activated(BallotNumber::new(2)).expect("check"));

        // Leader 2 completes Phase 1
        master
            .report_complete(BallotNumber::new(2), BallotNumber::new(1))
            .expect("complete");
        assert!(master.is_activated(BallotNumber::new(2)).expect("check"));

        // Reconfigure again: ballot 3
        master.new_ballot(make_config(3, 5003)).expect("new_ballot");
        master
            .report_complete(BallotNumber::new(3), BallotNumber::new(2))
            .expect("complete");
        assert!(master.is_activated(BallotNumber::new(3)).expect("check"));

        let active = master
            .current_config()
            .expect("config")
            .expect("should exist");
        assert_eq!(active.ballot, BallotNumber::new(3));
    }
}
