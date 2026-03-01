//! Membership provider: tracks which nodes are in the cluster.
//!
//! The [`MembershipProvider`] trait gives the actor system a view of the
//! current cluster topology. The placement strategy uses this to decide
//! where to activate new actors.
//!
//! # Design
//!
//! - `MembershipProvider` is a trait so implementations can range from a
//!   static list (simulation) to a gossip-based protocol (production).
//! - [`SharedMembership`] is a simple `Rc`-based implementation for
//!   simulation where all nodes share the same membership view.

use std::cell::RefCell;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::NetworkAddress;

/// Monotonically increasing membership version.
///
/// Every membership change (join, status transition, leave) bumps the version.
/// Used to detect stale snapshots and order membership updates.
///
/// # Orleans Reference
///
/// Corresponds to Orleans' `MembershipVersion` — a simple monotonic counter
/// that increases with every table write.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub struct MembershipVersion(pub u64);

impl MembershipVersion {
    /// Create version 0 (initial).
    pub fn new() -> Self {
        Self(0)
    }

    /// Return the next version.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for MembershipVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// Status of a node in the cluster.
///
/// Follows the Orleans silo lifecycle: nodes join, become active,
/// and eventually shut down or are declared dead by failure detection.
///
/// # Orleans Reference
///
/// Corresponds to Orleans' `SiloStatus` enum. We omit `Created` and `Stopping`
/// (Orleans-internal states) and keep only the states visible to the membership
/// protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeStatus {
    /// Node is joining the cluster (announced but not yet ready to serve).
    Joining,
    /// Node is fully operational and serving requests.
    Active,
    /// Node is gracefully shutting down (draining work).
    ShuttingDown,
    /// Node is no longer reachable (crashed or completed shutdown).
    Dead,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Joining => write!(f, "Joining"),
            Self::Active => write!(f, "Active"),
            Self::ShuttingDown => write!(f, "ShuttingDown"),
            Self::Dead => write!(f, "Dead"),
        }
    }
}

/// A single member of the cluster.
///
/// Combines a network address with its current lifecycle status.
///
/// # Orleans Reference
///
/// Corresponds to Orleans' `ClusterMember` (SiloAddress + SiloStatus + Name).
/// We use `NetworkAddress` instead of a separate `SiloAddress` type for now.
/// Generation (to distinguish restarts at the same address) can be added later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterMember {
    /// Network address of this member.
    pub address: NetworkAddress,
    /// Current lifecycle status.
    pub status: NodeStatus,
    /// Human-readable name (for logging/debugging).
    pub name: String,
}

impl ClusterMember {
    /// Create a new cluster member.
    pub fn new(address: NetworkAddress, status: NodeStatus, name: impl Into<String>) -> Self {
        Self {
            address,
            status,
            name: name.into(),
        }
    }

    /// Check if this member is in a status where it can serve requests.
    pub fn is_active(&self) -> bool {
        self.status == NodeStatus::Active
    }
}

/// Provides the current cluster membership view.
///
/// The simplest implementation is a static list of known addresses.
/// More advanced implementations can integrate with a gossip protocol
/// or service discovery system.
#[async_trait::async_trait(?Send)]
pub trait MembershipProvider: fmt::Debug {
    /// Returns the network addresses of all currently known members.
    async fn members(&self) -> Vec<NetworkAddress>;
}

/// Static membership backed by a shared `RefCell<Vec>`.
///
/// All nodes in a simulation can share the same `Rc<SharedMembership>`,
/// giving them a consistent view of the cluster. Members can be added
/// or removed dynamically during simulation.
///
/// # Example
///
/// ```rust,ignore
/// let membership = SharedMembership::new(vec![addr_a, addr_b]);
/// membership.add_member(addr_c);
/// assert_eq!(membership.members().await.len(), 3);
/// ```
#[derive(Debug)]
pub struct SharedMembership {
    members: RefCell<Vec<NetworkAddress>>,
}

impl SharedMembership {
    /// Create a new shared membership with the given initial members.
    pub fn new(members: Vec<NetworkAddress>) -> Self {
        Self {
            members: RefCell::new(members),
        }
    }

    /// Add a member to the cluster.
    pub fn add_member(&self, address: NetworkAddress) {
        let mut members = self.members.borrow_mut();
        if !members.contains(&address) {
            members.push(address);
        }
    }

    /// Remove a member from the cluster.
    ///
    /// Returns `true` if the member was present and removed.
    pub fn remove_member(&self, address: &NetworkAddress) -> bool {
        let mut members = self.members.borrow_mut();
        if let Some(pos) = members.iter().position(|a| a == address) {
            members.remove(pos);
            true
        } else {
            false
        }
    }
}

#[async_trait::async_trait(?Send)]
impl MembershipProvider for SharedMembership {
    async fn members(&self) -> Vec<NetworkAddress> {
        self.members.borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn addr(port: u16) -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[tokio::test]
    async fn test_shared_membership_initial() {
        let m = SharedMembership::new(vec![addr(4500), addr(4501)]);
        let members = m.members().await;
        assert_eq!(members.len(), 2);
    }

    #[tokio::test]
    async fn test_shared_membership_add() {
        let m = SharedMembership::new(vec![addr(4500)]);
        m.add_member(addr(4501));
        assert_eq!(m.members().await.len(), 2);
    }

    #[tokio::test]
    async fn test_shared_membership_add_duplicate() {
        let m = SharedMembership::new(vec![addr(4500)]);
        m.add_member(addr(4500));
        assert_eq!(m.members().await.len(), 1);
    }

    #[tokio::test]
    async fn test_shared_membership_remove() {
        let m = SharedMembership::new(vec![addr(4500), addr(4501)]);
        assert!(m.remove_member(&addr(4500)));
        assert_eq!(m.members().await.len(), 1);
        assert_eq!(m.members().await[0], addr(4501));
    }

    #[tokio::test]
    async fn test_shared_membership_remove_missing() {
        let m = SharedMembership::new(vec![addr(4500)]);
        assert!(!m.remove_member(&addr(9999)));
        assert_eq!(m.members().await.len(), 1);
    }

    #[test]
    fn test_membership_version_ordering() {
        let v0 = MembershipVersion::new();
        let v1 = v0.next();
        let v2 = v1.next();

        assert!(v0 < v1);
        assert!(v1 < v2);
        assert_eq!(v0.0, 0);
        assert_eq!(v1.0, 1);
        assert_eq!(v2.0, 2);
    }

    #[test]
    fn test_membership_version_display() {
        let v = MembershipVersion(42);
        assert_eq!(format!("{v}"), "v42");
    }

    #[test]
    fn test_node_status_display() {
        assert_eq!(format!("{}", NodeStatus::Joining), "Joining");
        assert_eq!(format!("{}", NodeStatus::Active), "Active");
        assert_eq!(format!("{}", NodeStatus::ShuttingDown), "ShuttingDown");
        assert_eq!(format!("{}", NodeStatus::Dead), "Dead");
    }

    #[test]
    fn test_cluster_member_is_active() {
        let active = ClusterMember::new(addr(4500), NodeStatus::Active, "node-0");
        assert!(active.is_active());

        let joining = ClusterMember::new(addr(4501), NodeStatus::Joining, "node-1");
        assert!(!joining.is_active());

        let shutting_down = ClusterMember::new(addr(4502), NodeStatus::ShuttingDown, "node-2");
        assert!(!shutting_down.is_active());

        let dead = ClusterMember::new(addr(4503), NodeStatus::Dead, "node-3");
        assert!(!dead.is_active());
    }
}
