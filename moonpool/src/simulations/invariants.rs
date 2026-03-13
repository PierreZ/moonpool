//! Reusable invariants for actor simulations.
//!
//! These invariants validate cross-component consistency and can be
//! used by any simulation that has a directory, membership, and node
//! actor tracking wired up via `StateHandle`.

use std::collections::{HashMap, HashSet};

use moonpool_sim::{Invariant, StateHandle, assert_always};

use crate::actors::{
    ActorAddress, ActorId, DIRECTORY_STATE_KEY, MEMBERSHIP_STATE_KEY, MembershipSnapshot,
    NodeStatus, node_actors_key,
};

/// Directory/membership/node consistency invariant.
///
/// Cross-checks three sources of truth:
/// 1. **Directory**: which node hosts each actor (`DIRECTORY_STATE_KEY`)
/// 2. **Membership**: which nodes are alive (`MEMBERSHIP_STATE_KEY`)
/// 3. **Per-node active actor sets** (`node_actors:{addr}`)
///
/// # Checks
///
/// 1. No directory entry points to a Dead node
/// 2. If directory says actor X is on node N, node N reports it active
/// 3. If node N has actor X active, directory points X to N
/// 4. No actor is active on two nodes simultaneously
///
/// # Usage
///
/// ```rust,ignore
/// use moonpool::simulations::invariants::DirectoryConsistency;
///
/// SimulationBuilder::new()
///     .invariant(DirectoryConsistency)
///     // ...
/// ```
#[derive(Debug, Clone, Copy)]
pub struct DirectoryConsistency;

impl Invariant for DirectoryConsistency {
    fn name(&self) -> &str {
        "directory_consistency"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        let Some(dir) = state.get::<HashMap<ActorId, ActorAddress>>(DIRECTORY_STATE_KEY) else {
            return;
        };
        let Some(membership) = state.get::<MembershipSnapshot>(MEMBERSHIP_STATE_KEY) else {
            return;
        };

        // Check 1: No directory entry points to a Dead node
        for (id, addr) in &dir {
            if let Some(status) = membership.status(&addr.endpoint.address) {
                assert_always!(
                    status != NodeStatus::Dead,
                    format!("directory entry for {} points to dead node", id.identity)
                );
            }
        }

        // Check 2: Directory → Node agreement
        // If directory says actor X is on node N, node N should report it active
        for (id, addr) in &dir {
            let key = node_actors_key(&addr.endpoint.address);
            if let Some(node_actors) = state.get::<HashSet<ActorId>>(&key) {
                assert_always!(
                    node_actors.contains(id),
                    format!(
                        "directory says {} is on {} but node doesn't have it",
                        id.identity, addr.endpoint.address
                    )
                );
            }
        }

        // Check 3: Node → Directory agreement
        // If node N has actor X active, directory should point X to N
        for (addr, member) in &membership.members {
            if !member.is_active() {
                continue;
            }
            let key = node_actors_key(addr);
            if let Some(node_actors) = state.get::<HashSet<ActorId>>(&key) {
                for id in &node_actors {
                    if let Some(dir_entry) = dir.get(id) {
                        assert_always!(
                            dir_entry.endpoint.address == *addr,
                            format!(
                                "node {} has {} but directory points to {}",
                                addr, id.identity, dir_entry.endpoint.address
                            )
                        );
                    }
                }
            }
        }

        // Check 4: Single activation — no actor active on two nodes
        let mut seen: HashMap<ActorId, crate::NetworkAddress> = HashMap::new();
        for (addr, member) in &membership.members {
            if !member.is_active() {
                continue;
            }
            let key = node_actors_key(addr);
            if let Some(node_actors) = state.get::<HashSet<ActorId>>(&key) {
                for id in node_actors {
                    if let Some(other_addr) = seen.get(&id) {
                        assert_always!(
                            false,
                            format!(
                                "actor {} active on both {} and {}",
                                id.identity, other_addr, addr
                            )
                        );
                    }
                    seen.insert(id, addr.clone());
                }
            }
        }
    }
}
