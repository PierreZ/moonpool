//! Actor directory: maps actor identities to network endpoints.
//!
//! The directory is the actor system's "phone book" — given an `ActorId`,
//! it returns the `Endpoint` where that actor is currently hosted.
//!
//! # Design
//!
//! - `ActorDirectory` is a trait so implementations can range from a simple
//!   in-memory map (single node) to a distributed directory (multi-node).
//! - The directory is queried by the `ActorRouter` on every call to resolve
//!   the target actor's location.
//! - Registration happens when an actor is activated on a node.
//!
//! # Orleans Reference
//!
//! This corresponds to Orleans' GrainDirectory / placement manager lookup.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;

use crate::Endpoint;

use super::ActorId;

/// Errors from directory operations.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    /// The actor was not found in the directory.
    #[error("actor not found: {id:?}")]
    NotFound {
        /// The actor identity that was not found.
        id: ActorId,
    },

    /// A registration conflict occurred (actor already registered elsewhere).
    #[error("actor already registered: {id:?}")]
    AlreadyRegistered {
        /// The conflicting actor identity.
        id: ActorId,
    },
}

/// Directory for resolving actor identities to network endpoints.
///
/// The directory maps `ActorId → Endpoint`, telling the caller which
/// node/transport hosts a given actor instance.
#[async_trait::async_trait(?Send)]
pub trait ActorDirectory: fmt::Debug {
    /// Look up the endpoint for an actor.
    ///
    /// Returns `Ok(Some(endpoint))` if the actor is registered,
    /// `Ok(None)` if the actor is not known to the directory.
    async fn lookup(&self, id: &ActorId) -> Result<Option<Endpoint>, DirectoryError>;

    /// Register an actor at a specific endpoint.
    ///
    /// Returns an error if the actor is already registered elsewhere.
    async fn register(&self, id: &ActorId, endpoint: Endpoint) -> Result<(), DirectoryError>;

    /// Remove an actor from the directory.
    ///
    /// Returns an error if the actor is not registered.
    async fn unregister(&self, id: &ActorId) -> Result<(), DirectoryError>;
}

/// Simple in-memory directory for single-node usage.
///
/// All lookups are O(1) HashMap operations. No network calls, no persistence.
/// Suitable for single-process actor systems and testing.
#[derive(Debug)]
pub struct InMemoryDirectory {
    entries: RefCell<HashMap<ActorId, Endpoint>>,
}

impl InMemoryDirectory {
    /// Create a new empty directory.
    pub fn new() -> Self {
        Self {
            entries: RefCell::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryDirectory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait(?Send)]
impl ActorDirectory for InMemoryDirectory {
    async fn lookup(&self, id: &ActorId) -> Result<Option<Endpoint>, DirectoryError> {
        Ok(self.entries.borrow().get(id).cloned())
    }

    async fn register(&self, id: &ActorId, endpoint: Endpoint) -> Result<(), DirectoryError> {
        let mut entries = self.entries.borrow_mut();
        if entries.contains_key(id) {
            return Err(DirectoryError::AlreadyRegistered { id: id.clone() });
        }
        entries.insert(id.clone(), endpoint);
        Ok(())
    }

    async fn unregister(&self, id: &ActorId) -> Result<(), DirectoryError> {
        let mut entries = self.entries.borrow_mut();
        if entries.remove(id).is_none() {
            return Err(DirectoryError::NotFound { id: id.clone() });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::{NetworkAddress, UID};

    use super::*;
    use crate::actors::ActorType;

    fn test_endpoint() -> Endpoint {
        Endpoint::new(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500),
            UID::new(0xBA4E_4B00, 0),
        )
    }

    fn alice_id() -> ActorId {
        ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "alice".to_string(),
        }
    }

    fn bob_id() -> ActorId {
        ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "bob".to_string(),
        }
    }

    #[tokio::test]
    async fn test_lookup_empty() {
        let dir = InMemoryDirectory::new();
        let result = dir.lookup(&alice_id()).await;
        assert!(result.is_ok());
        assert!(result.expect("lookup should succeed").is_none());
    }

    #[tokio::test]
    async fn test_register_and_lookup() {
        let dir = InMemoryDirectory::new();
        let endpoint = test_endpoint();

        dir.register(&alice_id(), endpoint.clone())
            .await
            .expect("register should succeed");

        let result = dir.lookup(&alice_id()).await.expect("lookup should succeed");
        assert_eq!(result, Some(endpoint));
    }

    #[tokio::test]
    async fn test_register_duplicate_fails() {
        let dir = InMemoryDirectory::new();
        let endpoint = test_endpoint();

        dir.register(&alice_id(), endpoint.clone())
            .await
            .expect("first register should succeed");

        let result = dir.register(&alice_id(), endpoint).await;
        assert!(matches!(
            result,
            Err(DirectoryError::AlreadyRegistered { .. })
        ));
    }

    #[tokio::test]
    async fn test_unregister() {
        let dir = InMemoryDirectory::new();
        let endpoint = test_endpoint();

        dir.register(&alice_id(), endpoint)
            .await
            .expect("register should succeed");

        dir.unregister(&alice_id())
            .await
            .expect("unregister should succeed");

        let result = dir.lookup(&alice_id()).await.expect("lookup should succeed");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_unregister_not_found() {
        let dir = InMemoryDirectory::new();

        let result = dir.unregister(&alice_id()).await;
        assert!(matches!(result, Err(DirectoryError::NotFound { .. })));
    }

    #[tokio::test]
    async fn test_multiple_actors() {
        let dir = InMemoryDirectory::new();
        let endpoint1 = Endpoint::new(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500),
            UID::new(0xBA4E_4B00, 0),
        );
        let endpoint2 = Endpoint::new(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4501),
            UID::new(0xBA4E_4B00, 0),
        );

        dir.register(&alice_id(), endpoint1.clone())
            .await
            .expect("register alice");
        dir.register(&bob_id(), endpoint2.clone())
            .await
            .expect("register bob");

        assert_eq!(
            dir.lookup(&alice_id()).await.expect("lookup alice"),
            Some(endpoint1)
        );
        assert_eq!(
            dir.lookup(&bob_id()).await.expect("lookup bob"),
            Some(endpoint2)
        );
    }
}
