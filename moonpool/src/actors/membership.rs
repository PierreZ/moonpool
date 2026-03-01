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

use crate::NetworkAddress;

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
}
