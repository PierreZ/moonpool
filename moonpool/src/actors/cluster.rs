//! Cluster configuration: shared state for all nodes in a cluster.
//!
//! [`ClusterConfig`] bundles the directory and membership provider that
//! nodes share. In simulation, all nodes reference the same `ClusterConfig`
//! via `Rc`, giving them a unified view of the cluster.
//!
//! # Example
//!
//! ```rust,ignore
//! let directory = Rc::new(InMemoryDirectory::new());
//! let membership = Rc::new(SharedMembership::new(vec![addr_a, addr_b]));
//!
//! let cluster = ClusterConfig::builder()
//!     .directory(directory)
//!     .membership(membership)
//!     .build()?;
//! ```

use std::rc::Rc;

use super::directory::ActorDirectory;
use super::membership::MembershipProvider;
use super::{InMemoryDirectory, SharedMembership};
use crate::NetworkAddress;

/// Shared cluster configuration for simulation.
///
/// In simulation, all nodes share the same `ClusterConfig` via `Rc`,
/// giving them a single shared directory and membership view.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    directory: Rc<dyn ActorDirectory>,
    membership: Rc<dyn MembershipProvider>,
}

impl ClusterConfig {
    /// Start building a cluster configuration.
    pub fn builder() -> ClusterConfigBuilder {
        ClusterConfigBuilder {
            directory: None,
            membership: None,
        }
    }

    /// Create a single-node cluster configuration.
    ///
    /// Uses an in-memory directory and a membership list containing
    /// only the given address.
    pub fn single_node(address: NetworkAddress) -> Self {
        Self {
            directory: Rc::new(InMemoryDirectory::new()),
            membership: Rc::new(SharedMembership::new(vec![address])),
        }
    }

    /// The shared actor directory.
    pub fn directory(&self) -> &Rc<dyn ActorDirectory> {
        &self.directory
    }

    /// The shared membership provider.
    pub fn membership(&self) -> &Rc<dyn MembershipProvider> {
        &self.membership
    }
}

/// Builder for [`ClusterConfig`].
pub struct ClusterConfigBuilder {
    directory: Option<Rc<dyn ActorDirectory>>,
    membership: Option<Rc<dyn MembershipProvider>>,
}

impl ClusterConfigBuilder {
    /// Set the actor directory.
    pub fn directory(mut self, directory: Rc<dyn ActorDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Set the membership provider.
    pub fn membership(mut self, membership: Rc<dyn MembershipProvider>) -> Self {
        self.membership = Some(membership);
        self
    }

    /// Build the cluster configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if directory or membership is not set.
    pub fn build(self) -> Result<ClusterConfig, ClusterConfigError> {
        let directory = self.directory.ok_or(ClusterConfigError::MissingDirectory)?;
        let membership = self
            .membership
            .ok_or(ClusterConfigError::MissingMembership)?;

        Ok(ClusterConfig {
            directory,
            membership,
        })
    }
}

/// Errors from building a [`ClusterConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ClusterConfigError {
    /// No directory was provided to the builder.
    #[error("cluster config requires a directory")]
    MissingDirectory,
    /// No membership provider was provided to the builder.
    #[error("cluster config requires a membership provider")]
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
    fn test_single_node() {
        let cluster = ClusterConfig::single_node(addr(4500));
        // Verify Debug is implemented
        let debug_str = format!("{:?}", cluster);
        assert!(!debug_str.is_empty());
        let _ = cluster.directory();
        let _ = cluster.membership();
    }

    #[test]
    fn test_builder() {
        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
        let membership: Rc<dyn MembershipProvider> =
            Rc::new(SharedMembership::new(vec![addr(4500)]));

        let cluster = ClusterConfig::builder()
            .directory(directory)
            .membership(membership)
            .build()
            .expect("build should succeed");

        let _ = cluster.directory();
        let _ = cluster.membership();
    }

    #[test]
    fn test_builder_missing_directory() {
        let membership: Rc<dyn MembershipProvider> =
            Rc::new(SharedMembership::new(vec![addr(4500)]));

        let result = ClusterConfig::builder().membership(membership).build();
        assert!(matches!(result, Err(ClusterConfigError::MissingDirectory)));
    }

    #[test]
    fn test_builder_missing_membership() {
        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());

        let result = ClusterConfig::builder().directory(directory).build();
        assert!(matches!(result, Err(ClusterConfigError::MissingMembership)));
    }

    #[tokio::test]
    async fn test_single_node_membership() {
        let cluster = ClusterConfig::single_node(addr(4500));
        let members = cluster.membership().members().await;
        assert_eq!(members.len(), 1);
        assert_eq!(members[0], addr(4500));
    }
}
