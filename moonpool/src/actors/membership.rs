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
use std::collections::HashMap;
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

/// Immutable snapshot of cluster membership at a specific version.
///
/// # Orleans Reference
///
/// Corresponds to Orleans' `ClusterMembershipSnapshot` — an immutable
/// dictionary of members keyed by address, plus a version number.
/// Orleans uses `ImmutableDictionary<SiloAddress, ClusterMember>`;
/// we use `HashMap` since we're single-threaded (`!Send`).
#[derive(Debug, Clone)]
pub struct MembershipSnapshot {
    /// All known members, keyed by network address.
    pub members: HashMap<NetworkAddress, ClusterMember>,
    /// Version of this snapshot (monotonically increasing).
    pub version: MembershipVersion,
}

impl MembershipSnapshot {
    /// Create an empty snapshot at version 0.
    pub fn new() -> Self {
        Self {
            members: HashMap::new(),
            version: MembershipVersion::new(),
        }
    }

    /// Get all members with a specific status.
    pub fn members_with_status(&self, status: NodeStatus) -> Vec<&ClusterMember> {
        self.members
            .values()
            .filter(|m| m.status == status)
            .collect()
    }

    /// Get all active member addresses (convenience for placement).
    pub fn active_members(&self) -> Vec<NetworkAddress> {
        self.members
            .values()
            .filter(|m| m.is_active())
            .map(|m| m.address.clone())
            .collect()
    }

    /// Get a specific member by address.
    pub fn get_member(&self, address: &NetworkAddress) -> Option<&ClusterMember> {
        self.members.get(address)
    }

    /// Get the status of a specific member, or `None` if unknown.
    pub fn get_status(&self, address: &NetworkAddress) -> Option<NodeStatus> {
        self.members.get(address).map(|m| m.status)
    }
}

impl Default for MembershipSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors from membership operations.
#[derive(Debug, thiserror::Error)]
pub enum MembershipError {
    /// The node was not found in the membership.
    #[error("node not found: {address}")]
    NotFound {
        /// The address that was not found.
        address: NetworkAddress,
    },
}

/// Provides the current cluster membership view and supports
/// node self-registration.
///
/// The simplest implementation is a shared in-memory list (simulation).
/// More advanced implementations can integrate with a gossip protocol,
/// service discovery system, or shared storage (Orleans-style).
///
/// # Orleans Reference
///
/// Combines aspects of Orleans' `IClusterMembershipService` (read
/// snapshot, subscribe to updates) and `MembershipAgent` (status
/// transitions for the local silo).
#[async_trait::async_trait(?Send)]
pub trait MembershipProvider: fmt::Debug {
    /// Returns the network addresses of all currently active members.
    ///
    /// This is the backward-compatible method used by placement strategies.
    /// Equivalent to `snapshot().active_members()`.
    async fn members(&self) -> Vec<NetworkAddress>;

    /// Returns a full membership snapshot with per-member status and version.
    async fn snapshot(&self) -> MembershipSnapshot;

    /// Register a node into the cluster with the given status.
    ///
    /// If the node is already registered, its status is updated.
    /// The membership version is bumped on every change.
    ///
    /// # Orleans Reference
    ///
    /// In Orleans, a silo registers itself by writing to the membership
    /// table during startup (`MembershipAgent.UpdateStatus`). This is the
    /// moonpool equivalent — the node announces itself.
    async fn register_node(
        &self,
        address: NetworkAddress,
        status: NodeStatus,
        name: String,
    ) -> Result<MembershipVersion, MembershipError>;

    /// Update the status of an already-registered node.
    ///
    /// Returns the new membership version, or an error if the node is
    /// not registered.
    async fn update_status(
        &self,
        address: &NetworkAddress,
        status: NodeStatus,
    ) -> Result<MembershipVersion, MembershipError>;
}

/// Shared in-memory membership for simulation.
///
/// All nodes in a simulation can share the same `Rc<SharedMembership>`,
/// giving them a consistent view of the cluster. Members register
/// themselves and transition through statuses.
///
/// # Orleans Reference
///
/// In Orleans, membership is stored in a shared table (Azure Table,
/// SQL, etc.) and gossiped. `SharedMembership` simulates this with
/// a shared `RefCell` — all nodes see the same data immediately
/// (no replication delay). Gossip delay can be simulated with buggify
/// in a later phase.
///
/// # Example
///
/// ```rust,ignore
/// let membership = SharedMembership::new();
/// membership.register_node(addr_a, NodeStatus::Active, "node-a".into()).await?;
/// membership.register_node(addr_b, NodeStatus::Active, "node-b".into()).await?;
/// assert_eq!(membership.members().await.len(), 2);
///
/// // Node leaves
/// membership.update_status(&addr_a, NodeStatus::Dead).await?;
/// assert_eq!(membership.members().await.len(), 1); // only active members
/// ```
#[derive(Debug)]
pub struct SharedMembership {
    inner: RefCell<SharedMembershipInner>,
}

#[derive(Debug)]
struct SharedMembershipInner {
    members: HashMap<NetworkAddress, ClusterMember>,
    version: MembershipVersion,
}

impl SharedMembership {
    /// Create a new empty shared membership.
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(SharedMembershipInner {
                members: HashMap::new(),
                version: MembershipVersion::new(),
            }),
        }
    }

    /// Create a shared membership pre-populated with active members.
    ///
    /// Convenience for tests and simple simulations. Each address is
    /// registered as `Active` with an auto-generated name.
    pub fn with_members(addresses: Vec<NetworkAddress>) -> Self {
        let mut members = HashMap::new();
        for (i, addr) in addresses.iter().enumerate() {
            members.insert(
                addr.clone(),
                ClusterMember::new(addr.clone(), NodeStatus::Active, format!("node-{}", i)),
            );
        }
        Self {
            inner: RefCell::new(SharedMembershipInner {
                members,
                version: MembershipVersion(addresses.len() as u64),
            }),
        }
    }

    /// Direct access: add a member (backward compatibility helper).
    ///
    /// Prefer `register_node()` via the trait for new code.
    pub fn add_member(&self, address: NetworkAddress) {
        let mut inner = self.inner.borrow_mut();
        if !inner.members.contains_key(&address) {
            let name = format!("node-{}", inner.members.len());
            inner.members.insert(
                address.clone(),
                ClusterMember::new(address, NodeStatus::Active, name),
            );
            inner.version = inner.version.next();
        }
    }

    /// Direct access: remove a member (backward compatibility helper).
    ///
    /// Prefer `update_status(_, NodeStatus::Dead)` via the trait for new code.
    pub fn remove_member(&self, address: &NetworkAddress) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.members.remove(address).is_some() {
            inner.version = inner.version.next();
            true
        } else {
            false
        }
    }
}

impl Default for SharedMembership {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait(?Send)]
impl MembershipProvider for SharedMembership {
    async fn members(&self) -> Vec<NetworkAddress> {
        self.inner
            .borrow()
            .members
            .values()
            .filter(|m| m.is_active())
            .map(|m| m.address.clone())
            .collect()
    }

    async fn snapshot(&self) -> MembershipSnapshot {
        let inner = self.inner.borrow();
        MembershipSnapshot {
            members: inner.members.clone(),
            version: inner.version,
        }
    }

    async fn register_node(
        &self,
        address: NetworkAddress,
        status: NodeStatus,
        name: String,
    ) -> Result<MembershipVersion, MembershipError> {
        let mut inner = self.inner.borrow_mut();
        inner.version = inner.version.next();
        inner
            .members
            .insert(address.clone(), ClusterMember::new(address, status, name));
        Ok(inner.version)
    }

    async fn update_status(
        &self,
        address: &NetworkAddress,
        status: NodeStatus,
    ) -> Result<MembershipVersion, MembershipError> {
        let mut inner = self.inner.borrow_mut();
        match inner.members.get_mut(address) {
            Some(member) => {
                member.status = status;
                inner.version = inner.version.next();
                Ok(inner.version)
            }
            None => Err(MembershipError::NotFound {
                address: address.clone(),
            }),
        }
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
    async fn test_shared_membership_add() {
        let m = SharedMembership::new();
        m.add_member(addr(4500));
        m.add_member(addr(4501));
        assert_eq!(m.members().await.len(), 2);
    }

    #[tokio::test]
    async fn test_shared_membership_add_duplicate() {
        let m = SharedMembership::new();
        m.add_member(addr(4500));
        m.add_member(addr(4500));
        assert_eq!(m.members().await.len(), 1);
    }

    #[tokio::test]
    async fn test_shared_membership_remove() {
        let m = SharedMembership::with_members(vec![addr(4500), addr(4501)]);
        assert!(m.remove_member(&addr(4500)));
        assert_eq!(m.members().await.len(), 1);
    }

    #[tokio::test]
    async fn test_shared_membership_remove_missing() {
        let m = SharedMembership::with_members(vec![addr(4500)]);
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

    #[tokio::test]
    async fn test_register_node_and_snapshot() {
        let m = SharedMembership::new();
        let v = m
            .register_node(addr(4500), NodeStatus::Active, "node-a".into())
            .await
            .expect("register should succeed");
        assert_eq!(v, MembershipVersion(1));

        let snap = m.snapshot().await;
        assert_eq!(snap.version, MembershipVersion(1));
        assert_eq!(snap.members.len(), 1);
        let member = snap.get_member(&addr(4500)).expect("member should exist");
        assert_eq!(member.status, NodeStatus::Active);
        assert_eq!(member.name, "node-a");
    }

    #[tokio::test]
    async fn test_update_status() {
        let m = SharedMembership::new();
        m.register_node(addr(4500), NodeStatus::Active, "node-a".into())
            .await
            .expect("register");

        let v = m
            .update_status(&addr(4500), NodeStatus::Dead)
            .await
            .expect("update should succeed");
        assert_eq!(v, MembershipVersion(2));

        // members() should exclude dead nodes
        assert_eq!(m.members().await.len(), 0);

        // snapshot still has the entry
        let snap = m.snapshot().await;
        assert_eq!(snap.get_status(&addr(4500)), Some(NodeStatus::Dead));
    }

    #[tokio::test]
    async fn test_members_returns_only_active() {
        let m = SharedMembership::new();
        m.register_node(addr(4500), NodeStatus::Active, "a".into())
            .await
            .expect("register");
        m.register_node(addr(4501), NodeStatus::Joining, "b".into())
            .await
            .expect("register");
        m.register_node(addr(4502), NodeStatus::Dead, "c".into())
            .await
            .expect("register");
        m.register_node(addr(4503), NodeStatus::ShuttingDown, "d".into())
            .await
            .expect("register");

        let active = m.members().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0], addr(4500));
    }

    #[tokio::test]
    async fn test_version_increments() {
        let m = SharedMembership::new();
        let v1 = m
            .register_node(addr(4500), NodeStatus::Active, "a".into())
            .await
            .expect("register");
        let v2 = m
            .register_node(addr(4501), NodeStatus::Active, "b".into())
            .await
            .expect("register");
        let v3 = m
            .update_status(&addr(4500), NodeStatus::Dead)
            .await
            .expect("update");

        assert_eq!(v1, MembershipVersion(1));
        assert_eq!(v2, MembershipVersion(2));
        assert_eq!(v3, MembershipVersion(3));
    }

    #[tokio::test]
    async fn test_with_members_convenience() {
        let m = SharedMembership::with_members(vec![addr(4500), addr(4501)]);
        let members = m.members().await;
        assert_eq!(members.len(), 2);

        let snap = m.snapshot().await;
        assert_eq!(snap.version, MembershipVersion(2));
        assert!(snap.get_member(&addr(4500)).expect("exists").is_active());
        assert!(snap.get_member(&addr(4501)).expect("exists").is_active());
    }

    #[tokio::test]
    async fn test_update_status_not_found() {
        let m = SharedMembership::new();
        let result = m.update_status(&addr(9999), NodeStatus::Dead).await;
        assert!(matches!(result, Err(MembershipError::NotFound { .. })));
    }
}
