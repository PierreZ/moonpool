//! Per-node configuration for [`MoonpoolNode`](super::MoonpoolNode).
//!
//! Separates node-level settings (address, placement, state store)
//! from cluster-level settings ([`ClusterConfig`](super::ClusterConfig)).

use std::rc::Rc;

use crate::NetworkAddress;

use super::placement::PlacementStrategy;
use super::state::ActorStateStore;

/// Per-node configuration for a [`MoonpoolNode`](super::MoonpoolNode).
///
/// Separates node-level settings (address, placement, state store)
/// from cluster-level settings ([`ClusterConfig`](super::ClusterConfig)).
///
/// # Example
///
/// ```rust,ignore
/// // Single-node: address inferred from topology
/// let config = NodeConfig::default();
///
/// // Multi-node: specify which address this node binds to
/// let config = NodeConfig::for_address(addr);
///
/// // Full control via builder
/// let config = NodeConfig::builder()
///     .address(addr)
///     .state_store(store)
///     .build();
/// ```
#[derive(Debug, Clone, Default)]
pub struct NodeConfig {
    address: Option<NetworkAddress>,
    placement: Option<Rc<dyn PlacementStrategy>>,
    state_store: Option<Rc<dyn ActorStateStore>>,
}

impl NodeConfig {
    /// Create a node config with only the address set.
    ///
    /// Use this in multi-node clusters to specify which topology address
    /// this node binds to.
    pub fn for_address(address: NetworkAddress) -> Self {
        Self {
            address: Some(address),
            placement: None,
            state_store: None,
        }
    }

    /// Start building a node configuration.
    pub fn builder() -> NodeConfigBuilder {
        NodeConfigBuilder::default()
    }

    /// The node's network address, if explicitly set.
    pub fn address(&self) -> Option<&NetworkAddress> {
        self.address.as_ref()
    }

    /// The placement strategy, if explicitly set.
    pub fn placement(&self) -> Option<&Rc<dyn PlacementStrategy>> {
        self.placement.as_ref()
    }

    /// The actor state store, if explicitly set.
    pub fn state_store(&self) -> Option<&Rc<dyn ActorStateStore>> {
        self.state_store.as_ref()
    }
}

/// Builder for [`NodeConfig`].
#[derive(Debug, Clone, Default)]
pub struct NodeConfigBuilder {
    address: Option<NetworkAddress>,
    placement: Option<Rc<dyn PlacementStrategy>>,
    state_store: Option<Rc<dyn ActorStateStore>>,
}

impl NodeConfigBuilder {
    /// Set the node's network address.
    pub fn address(mut self, address: NetworkAddress) -> Self {
        self.address = Some(address);
        self
    }

    /// Set the placement strategy.
    pub fn placement(mut self, placement: Rc<dyn PlacementStrategy>) -> Self {
        self.placement = Some(placement);
        self
    }

    /// Set the actor state store.
    pub fn state_store(mut self, store: Rc<dyn ActorStateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Build the node configuration (infallible).
    pub fn build(self) -> NodeConfig {
        NodeConfig {
            address: self.address,
            placement: self.placement,
            state_store: self.state_store,
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

    #[test]
    fn test_default() {
        let config = NodeConfig::default();
        assert!(config.address().is_none());
        assert!(config.placement().is_none());
        assert!(config.state_store().is_none());
    }

    #[test]
    fn test_for_address() {
        let config = NodeConfig::for_address(addr(4500));
        assert_eq!(config.address(), Some(&addr(4500)));
        assert!(config.placement().is_none());
        assert!(config.state_store().is_none());
    }

    #[test]
    fn test_builder() {
        let config = NodeConfig::builder().address(addr(4500)).build();
        assert_eq!(config.address(), Some(&addr(4500)));
    }
}
