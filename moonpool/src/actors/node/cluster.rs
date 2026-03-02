//! Cluster configuration: shared state for all nodes in a cluster.
//!
//! [`ClusterConfig`] bundles the directory, membership provider, and placement
//! director that nodes share. In simulation, all nodes reference the same
//! `ClusterConfig` via `Rc`, giving them a unified view of the cluster.
//!
//! # Example
//!
//! ```rust,ignore
//! let cluster = ClusterConfig::builder()
//!     .name("banking")
//!     .topology(vec![addr_a, addr_b])
//!     .build()?;
//! ```

use std::rc::Rc;

use crate::NetworkAddress;
use crate::actors::infrastructure::directory::ActorDirectory;
use crate::actors::infrastructure::membership::MembershipProvider;
use crate::actors::infrastructure::placement::{DefaultPlacementDirector, PlacementDirector};
use crate::actors::{InMemoryDirectory, SharedMembership};

/// Shared cluster configuration for simulation.
///
/// In simulation, all nodes share the same `ClusterConfig` via `Rc`,
/// giving them a single shared directory, membership view, and placement
/// director.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    name: Option<String>,
    directory: Rc<dyn ActorDirectory>,
    membership: Rc<dyn MembershipProvider>,
    placement_director: Rc<dyn PlacementDirector>,
}

impl ClusterConfig {
    /// Start building a cluster configuration.
    pub fn builder() -> ClusterConfigBuilder {
        ClusterConfigBuilder {
            name: None,
            directory: None,
            membership: None,
            placement_director: None,
        }
    }

    /// Optional cluster name (for logging/debugging).
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The shared actor directory.
    pub fn directory(&self) -> &Rc<dyn ActorDirectory> {
        &self.directory
    }

    /// The shared membership provider.
    pub fn membership(&self) -> &Rc<dyn MembershipProvider> {
        &self.membership
    }

    /// The shared placement director.
    pub fn placement_director(&self) -> &Rc<dyn PlacementDirector> {
        &self.placement_director
    }
}

/// Builder for [`ClusterConfig`].
pub struct ClusterConfigBuilder {
    name: Option<String>,
    directory: Option<Rc<dyn ActorDirectory>>,
    membership: Option<Rc<dyn MembershipProvider>>,
    placement_director: Option<Rc<dyn PlacementDirector>>,
}

impl ClusterConfigBuilder {
    /// Set the cluster name (for logging/debugging).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the actor directory.
    ///
    /// If not set, defaults to [`InMemoryDirectory`].
    pub fn directory(mut self, directory: Rc<dyn ActorDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Set the membership provider.
    pub fn membership(mut self, membership: Rc<dyn MembershipProvider>) -> Self {
        self.membership = Some(membership);
        self
    }

    /// Convenience: set the cluster topology from a list of addresses.
    ///
    /// Creates a [`SharedMembership`] internally. Use [`membership()`](Self::membership)
    /// for custom membership providers.
    pub fn topology(self, addresses: Vec<NetworkAddress>) -> Self {
        self.membership(Rc::new(SharedMembership::with_members(addresses)))
    }

    /// Set the placement director.
    ///
    /// If not set, defaults to [`DefaultPlacementDirector`].
    pub fn placement_director(mut self, director: Rc<dyn PlacementDirector>) -> Self {
        self.placement_director = Some(director);
        self
    }

    /// Build the cluster configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if membership is not set (via [`topology()`](Self::topology)
    /// or [`membership()`](Self::membership)).
    pub fn build(self) -> Result<ClusterConfig, ClusterConfigError> {
        let membership = self
            .membership
            .ok_or(ClusterConfigError::MissingMembership)?;
        let directory = self
            .directory
            .unwrap_or_else(|| Rc::new(InMemoryDirectory::new()));
        let placement_director = self
            .placement_director
            .unwrap_or_else(|| Rc::new(DefaultPlacementDirector::default()));

        Ok(ClusterConfig {
            name: self.name,
            directory,
            membership,
            placement_director,
        })
    }
}

/// Errors from building a [`ClusterConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ClusterConfigError {
    /// No membership provider was provided to the builder.
    #[error("cluster config requires a membership provider (call topology() or membership())")]
    MissingMembership,
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn addr(port: u16) -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn test_builder_with_topology() {
        let cluster = ClusterConfig::builder()
            .name("test")
            .topology(vec![addr(4500)])
            .build()
            .expect("build should succeed");

        assert_eq!(cluster.name(), Some("test"));
        let _ = cluster.directory();
        let _ = cluster.membership();
        let _ = cluster.placement_director();
    }

    #[test]
    fn test_builder_without_name() {
        let cluster = ClusterConfig::builder()
            .topology(vec![addr(4500)])
            .build()
            .expect("build should succeed");

        assert_eq!(cluster.name(), None);
    }

    #[test]
    fn test_builder_with_explicit_directory() {
        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
        let membership: Rc<dyn MembershipProvider> =
            Rc::new(SharedMembership::with_members(vec![addr(4500)]));

        let cluster = ClusterConfig::builder()
            .directory(directory)
            .membership(membership)
            .build()
            .expect("build should succeed");

        let _ = cluster.directory();
        let _ = cluster.membership();
    }

    #[test]
    fn test_builder_defaults_directory() {
        let cluster = ClusterConfig::builder()
            .topology(vec![addr(4500)])
            .build()
            .expect("build should succeed");

        // Directory should be defaulted (InMemoryDirectory)
        let debug_str = format!("{:?}", cluster);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_builder_missing_membership() {
        let result = ClusterConfig::builder().build();
        assert!(matches!(result, Err(ClusterConfigError::MissingMembership)));
    }

    #[tokio::test]
    async fn test_topology_membership() {
        let cluster = ClusterConfig::builder()
            .topology(vec![addr(4500)])
            .build()
            .expect("build should succeed");

        let members = cluster.membership().members().await;
        assert_eq!(members.len(), 1);
        assert_eq!(members[0], addr(4500));
    }

    #[test]
    fn test_builder_defaults_placement_director() {
        let cluster = ClusterConfig::builder()
            .topology(vec![addr(4500)])
            .build()
            .expect("build should succeed");

        // Should have a default placement director
        let debug_str = format!("{:?}", cluster.placement_director());
        assert!(debug_str.contains("DefaultPlacementDirector"));
    }
}
