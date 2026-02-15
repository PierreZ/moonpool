//! Reconfiguration logic for Vertical Paxos II.
//!
//! When the master detects that the current primary has failed (via heartbeat
//! timeout), it triggers reconfiguration: picking a new leader, issuing a new
//! ballot, and orchestrating the activation protocol.
//!
//! ## The Key VP II Insight: New Leader IS a Read Quorum
//!
//! In VP II, the write quorum is ALL acceptors. This means every acceptor
//! has seen every chosen value. When the master picks a surviving acceptor
//! as the new leader, that acceptor **already has all committed state locally**.
//! No state transfer is needed — the new leader is its own "read quorum."
//!
//! Compare with Raft: when a new leader is elected, it must discover which
//! log entries are committed by communicating with a majority of followers.
//! In VP II, the new leader skips this entirely because it IS a complete
//! copy of the committed state.
//!
//! ## Reconfiguration Flow
//!
//! ```text
//! Master                          New Leader (surviving acceptor)
//!   │                                │
//!   │  (1) Detect primary failure    │
//!   │      (heartbeat timeout)       │
//!   │                                │
//!   │  (2) Pick new leader from      │
//!   │      surviving acceptors       │
//!   │                                │
//!   │── new_ballot(new_config) ─────>│  (3) Issue new ballot
//!   │                                │
//!   │                                │  (4) Phase 1 from prev config
//!   │                                │      (new leader queries itself
//!   │                                │       and other prev acceptors)
//!   │                                │
//!   │                                │  (5) Re-commit any bound values
//!   │                                │
//!   │<── report_complete(bal) ───────│  (6) Report to master
//!   │                                │
//!   │── activated(bal) ─────────────>│  (7) Master activates
//!   │                                │
//!   │                                │  (8) Leader begins serving clients
//! ```
//!
//! ## Safety During Reconfiguration
//!
//! The activation protocol ensures that at most one leader is active at any
//! time. The old leader either:
//! - Already crashed (hence the reconfiguration), or
//! - Will discover its ballot is stale when it tries to send Phase 2a messages
//!   (acceptors will have moved to the higher ballot from Phase 1a).
//!
//! Either way, the old leader cannot successfully commit new values, so there
//! is no split-brain.

use moonpool_core::NetworkAddress;
use tracing::info;

use crate::master::ConfigurationMaster;
use crate::types::{BallotNumber, Configuration, PaxosError};

/// Pick a new leader from the surviving acceptors.
///
/// The selection strategy is simple: pick the first acceptor that is NOT
/// the failed leader. In a production system, you might consider load,
/// locality, or other factors.
///
/// ## Why any acceptor works
///
/// Since VP II's write quorum = ALL, every surviving acceptor has a complete
/// copy of all committed values. So any surviving acceptor can serve as the
/// new leader without needing state transfer. This is the key insight of VP II.
///
/// In Raft, the equivalent would be: pick the follower with the most complete
/// log. But in VP II, all followers have equally complete logs (because
/// write quorum = all).
///
/// # Arguments
///
/// * `acceptors` - The current set of acceptors
/// * `failed_leader` - The address of the leader that failed
///
/// # Returns
///
/// The address of the new leader, or `None` if no surviving acceptor is found.
pub fn pick_new_leader(
    acceptors: &[NetworkAddress],
    failed_leader: &NetworkAddress,
) -> Option<NetworkAddress> {
    acceptors
        .iter()
        .find(|addr| *addr != failed_leader)
        .cloned()
}

/// Trigger a reconfiguration: issue a new ballot with a new leader.
///
/// Called by the master when it detects primary failure. This:
/// 1. Picks a new leader from surviving acceptors
/// 2. Creates a new configuration with a higher ballot
/// 3. Registers it with the master as pending (not yet activated)
///
/// The caller is then responsible for:
/// - Notifying the new leader to start Phase 1
/// - Waiting for the leader to report_complete
/// - Activating the new configuration
///
/// # Arguments
///
/// * `master` - The configuration master
/// * `failed_leader` - Address of the failed primary
/// * `current_acceptors` - Current set of acceptors (unchanged by reconfig)
///
/// # Returns
///
/// The new `Configuration` that was registered with the master, or an error
/// if no suitable leader could be found.
pub fn trigger_reconfiguration(
    master: &mut dyn ConfigurationMaster,
    failed_leader: &NetworkAddress,
    current_acceptors: &[NetworkAddress],
) -> Result<Configuration, PaxosError> {
    // Step 1: Pick a new leader from surviving acceptors
    let new_leader = pick_new_leader(current_acceptors, failed_leader).ok_or_else(|| {
        PaxosError::Network("no surviving acceptor available as new leader".to_string())
    })?;

    // Step 2: Determine the next ballot number
    let current_config = master.current_config()?;
    let next_ballot = match &current_config {
        Some(config) => config.ballot.next(),
        None => BallotNumber::new(1),
    };

    // Step 3: Create and register the new configuration
    let new_config = Configuration {
        ballot: next_ballot,
        leader: new_leader.clone(),
        acceptors: current_acceptors.to_vec(),
    };

    info!(
        new_ballot = %next_ballot,
        new_leader = %new_leader,
        failed_leader = %failed_leader,
        "triggering reconfiguration: new leader selected from surviving acceptors"
    );

    master.new_ballot(new_config.clone())?;

    Ok(new_config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master::InMemoryConfigMaster;
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
    fn test_pick_new_leader_excludes_failed() {
        let acceptors = vec![make_addr(5001), make_addr(5002), make_addr(5003)];
        let failed = make_addr(5001);

        let new_leader = pick_new_leader(&acceptors, &failed);
        assert!(new_leader.is_some());
        let leader = new_leader.expect("should find leader");
        assert_ne!(leader, failed);
    }

    #[test]
    fn test_pick_new_leader_no_survivors() {
        let acceptors = vec![make_addr(5001)];
        let failed = make_addr(5001);

        let new_leader = pick_new_leader(&acceptors, &failed);
        assert!(new_leader.is_none());
    }

    #[test]
    fn test_pick_new_leader_failed_not_in_list() {
        let acceptors = vec![make_addr(5001), make_addr(5002)];
        let failed = make_addr(9999); // not in the list

        let new_leader = pick_new_leader(&acceptors, &failed);
        assert!(new_leader.is_some());
    }

    #[test]
    fn test_trigger_reconfiguration() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(1, 5001)).expect("bootstrap");

        let acceptors = vec![make_addr(5001), make_addr(5002), make_addr(5003)];
        let failed = make_addr(5001);

        let new_config =
            trigger_reconfiguration(&mut master, &failed, &acceptors).expect("reconfig");

        // New ballot should be 2
        assert_eq!(new_config.ballot, BallotNumber::new(2));

        // New leader should NOT be the failed one
        assert_ne!(new_config.leader, failed);

        // Acceptors should be unchanged
        assert_eq!(new_config.acceptors, acceptors);

        // The new config should be pending (not yet activated)
        assert!(!master.is_activated(BallotNumber::new(2)).expect("check"));

        // Old config should still be active
        assert!(master.is_activated(BallotNumber::new(1)).expect("check"));
    }

    #[test]
    fn test_trigger_reconfiguration_twice() {
        let mut master = InMemoryConfigMaster::new();
        master.bootstrap(make_config(1, 5001)).expect("bootstrap");

        let acceptors = vec![make_addr(5001), make_addr(5002), make_addr(5003)];

        // First reconfig: 5001 fails
        let config2 =
            trigger_reconfiguration(&mut master, &make_addr(5001), &acceptors).expect("reconfig1");
        assert_eq!(config2.ballot, BallotNumber::new(2));

        // Complete activation so we can trigger another reconfig
        master
            .report_complete(BallotNumber::new(2), BallotNumber::new(1))
            .expect("complete");

        // Second reconfig: new leader fails
        let config3 =
            trigger_reconfiguration(&mut master, &config2.leader, &acceptors).expect("reconfig2");
        assert_eq!(config3.ballot, BallotNumber::new(3));
        assert_ne!(config3.leader, config2.leader);
    }
}
