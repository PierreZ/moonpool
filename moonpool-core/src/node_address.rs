//! Node address: identifies a specific node instance in the cluster.
//!
//! A [`NodeAddress`] combines a [`NetworkAddress`] with a generation counter,
//! allowing the system to distinguish between different incarnations of a node
//! at the same IP:port (e.g., after a restart).

use serde::{Deserialize, Serialize};

use crate::NetworkAddress;

/// Identifies a specific node instance in the cluster.
///
/// The `generation` distinguishes node restarts at the same network address.
/// In simulation, this comes from the RNG seed; in production, from a
/// monotonic timestamp or similar mechanism.
///
/// # Examples
///
/// ```
/// use moonpool_core::{NodeAddress, NetworkAddress};
/// use std::net::{IpAddr, Ipv4Addr};
///
/// let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
/// let node = NodeAddress::new(addr, 1);
/// assert_eq!(node.generation(), 1);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeAddress {
    address: NetworkAddress,
    generation: u64,
}

impl NodeAddress {
    /// Create a new node address.
    pub fn new(address: NetworkAddress, generation: u64) -> Self {
        Self {
            address,
            generation,
        }
    }

    /// The network address of this node.
    pub fn address(&self) -> &NetworkAddress {
        &self.address
    }

    /// The generation counter for this node instance.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl std::fmt::Display for NodeAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@gen{}", self.address, self.generation)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn test_node_address_equality() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let n1 = NodeAddress::new(addr.clone(), 1);
        let n2 = NodeAddress::new(addr.clone(), 1);
        let n3 = NodeAddress::new(addr, 2);

        assert_eq!(n1, n2);
        assert_ne!(n1, n3);
    }

    #[test]
    fn test_node_address_accessors() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let node = NodeAddress::new(addr.clone(), 42);

        assert_eq!(node.address(), &addr);
        assert_eq!(node.generation(), 42);
    }

    #[test]
    fn test_node_address_display() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let node = NodeAddress::new(addr, 7);

        assert_eq!(node.to_string(), "127.0.0.1:4500@gen7");
    }

    #[test]
    fn test_node_address_serde_roundtrip() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let node = NodeAddress::new(addr, 42);

        let json = serde_json::to_string(&node).expect("serialize");
        let decoded: NodeAddress = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(node, decoded);
    }

    #[test]
    fn test_node_address_hash_works_in_collections() {
        use std::collections::HashSet;

        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let mut set = HashSet::new();
        set.insert(NodeAddress::new(addr.clone(), 1));
        set.insert(NodeAddress::new(addr.clone(), 2));
        set.insert(NodeAddress::new(addr, 1)); // duplicate

        assert_eq!(set.len(), 2);
    }
}
