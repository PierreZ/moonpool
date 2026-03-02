//! Actor directory: maps actor identities to network endpoints.
//!
//! The directory is the actor system's "phone book" — given an `ActorId`,
//! it returns the `ActorAddress` where that actor is currently hosted.
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
//! This corresponds to Orleans' `IGrainDirectory`:
//! - `Register(GrainAddress)` → returns existing on conflict
//! - `Lookup(GrainId)` → returns `GrainAddress?`
//! - `Unregister(GrainAddress)` → removes only if activation ID matches
//! - `UnregisterSilos(List<SiloAddress>)` → batch cleanup on node death

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;

use crate::NetworkAddress;

use crate::actors::types::{ActorAddress, ActorId};

/// Errors from directory operations.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    /// The actor was not found in the directory.
    #[error("actor not found: {id:?}")]
    NotFound {
        /// The actor identity that was not found.
        id: ActorId,
    },
}

/// Directory for resolving actor identities to their activation addresses.
///
/// The directory maps `ActorId → ActorAddress`, telling the caller which
/// node hosts a given actor instance and which activation is current.
///
/// # Register Semantics (Orleans-style)
///
/// `register()` is idempotent with conflict detection:
/// - If no entry exists: registers the new address, returns it
/// - If an entry already exists: does NOT overwrite, returns the existing entry
///
/// The caller compares the returned `ActorAddress` with what it tried to
/// register. If they differ, another activation won — the caller should
/// use the returned address instead.
///
/// # Orleans Reference
///
/// Corresponds to Orleans' `IGrainDirectory`:
/// - `Register(GrainAddress)` → returns existing `GrainAddress` on conflict
/// - `Lookup(GrainId)` → returns `GrainAddress?`
/// - `Unregister(GrainAddress)` → removes only if activation ID matches
/// - `UnregisterSilos(List<SiloAddress>)` → batch cleanup on node death
#[async_trait::async_trait(?Send)]
pub trait ActorDirectory: fmt::Debug {
    /// Look up the current address for an actor.
    ///
    /// Returns `Ok(Some(address))` if the actor is registered,
    /// `Ok(None)` if the actor is not known to the directory.
    async fn lookup(&self, id: &ActorId) -> Result<Option<ActorAddress>, DirectoryError>;

    /// Register an actor activation in the directory.
    ///
    /// If no entry exists for this actor, the address is registered and
    /// returned. If an entry already exists, the existing address is
    /// returned WITHOUT overwriting it.
    ///
    /// The caller MUST compare the returned address with what it
    /// registered. If they differ (`returned.activation_id != address.activation_id`),
    /// another node won the race.
    ///
    /// # Orleans Reference
    ///
    /// This matches Orleans' `IGrainDirectory.Register()` semantics:
    /// "If there is already an existing entry, the directory will not
    /// override it."
    async fn register(&self, address: ActorAddress) -> Result<ActorAddress, DirectoryError>;

    /// Remove an actor from the directory.
    ///
    /// Only removes the entry if the activation ID matches the one
    /// currently registered. This prevents a node from accidentally
    /// removing a newer activation registered by another node.
    ///
    /// Returns `Ok(())` if the entry was removed or didn't exist.
    /// This is intentionally lenient — Orleans' unregister is also
    /// a best-effort operation.
    async fn unregister(&self, address: &ActorAddress) -> Result<(), DirectoryError>;

    /// Remove all directory entries pointing at the given member addresses.
    ///
    /// Called when nodes are declared dead. Removes all actor registrations
    /// hosted on those nodes so that actors can be re-activated elsewhere.
    ///
    /// # Orleans Reference
    ///
    /// Corresponds to Orleans' `IGrainDirectory.UnregisterSilos()`:
    /// "Unregister from the directory all entries that point to any of
    /// the silos in the argument."
    async fn unregister_members(
        &self,
        addresses: &[NetworkAddress],
    ) -> Result<Vec<ActorAddress>, DirectoryError>;

    /// List all actor entries currently in the directory.
    ///
    /// Returns all registered `ActorAddress` entries. Useful for debugging,
    /// monitoring, and tooling. The order of returned entries is unspecified.
    async fn list_all(&self) -> Result<Vec<ActorAddress>, DirectoryError>;
}

/// Simple in-memory directory for single-node and simulation usage.
///
/// All lookups are O(1) HashMap operations. No network calls, no persistence.
/// Suitable for single-process actor systems and testing.
///
/// # Orleans Reference
///
/// Equivalent to a single-partition in-memory `IGrainDirectory`.
/// Orleans' production directory (`DistributedGrainDirectory`) partitions
/// across silos using consistent hashing — that layer sits above this.
#[derive(Debug)]
pub struct InMemoryDirectory {
    entries: RefCell<HashMap<ActorId, ActorAddress>>,
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
    async fn lookup(&self, id: &ActorId) -> Result<Option<ActorAddress>, DirectoryError> {
        Ok(self.entries.borrow().get(id).cloned())
    }

    async fn register(&self, address: ActorAddress) -> Result<ActorAddress, DirectoryError> {
        let mut entries = self.entries.borrow_mut();
        match entries.get(&address.actor_id) {
            Some(existing) => {
                // Entry already exists — return existing WITHOUT overwriting (Orleans semantics)
                Ok(existing.clone())
            }
            None => {
                entries.insert(address.actor_id.clone(), address.clone());
                Ok(address)
            }
        }
    }

    async fn unregister(&self, address: &ActorAddress) -> Result<(), DirectoryError> {
        let mut entries = self.entries.borrow_mut();
        // Only remove if the activation ID matches
        if let Some(existing) = entries.get(&address.actor_id)
            && existing.activation_id == address.activation_id
        {
            entries.remove(&address.actor_id);
        }
        // If not found, that's fine too — idempotent
        Ok(())
    }

    async fn unregister_members(
        &self,
        addresses: &[NetworkAddress],
    ) -> Result<Vec<ActorAddress>, DirectoryError> {
        let mut entries = self.entries.borrow_mut();
        let mut removed = Vec::new();
        entries.retain(|_id, addr| {
            if addresses.contains(&addr.endpoint.address) {
                removed.push(addr.clone());
                false
            } else {
                true
            }
        });
        Ok(removed)
    }

    async fn list_all(&self) -> Result<Vec<ActorAddress>, DirectoryError> {
        Ok(self.entries.borrow().values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::{NetworkAddress, UID};

    use super::*;
    use crate::Endpoint;
    use crate::actors::types::{ActivationId, ActorType};

    fn test_endpoint() -> Endpoint {
        Endpoint::new(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500),
            UID::new(0xBA4E_4B00, 0),
        )
    }

    fn test_endpoint_at(port: u16) -> Endpoint {
        Endpoint::new(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
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

    fn alice_address(activation: u64) -> ActorAddress {
        ActorAddress::new(alice_id(), test_endpoint(), ActivationId::new(activation))
    }

    fn bob_address(activation: u64) -> ActorAddress {
        ActorAddress::new(bob_id(), test_endpoint(), ActivationId::new(activation))
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
        let addr = alice_address(1);

        let registered = dir
            .register(addr.clone())
            .await
            .expect("register should succeed");

        assert_eq!(registered, addr);

        let result = dir
            .lookup(&alice_id())
            .await
            .expect("lookup should succeed");
        assert_eq!(result, Some(addr));
    }

    #[tokio::test]
    async fn test_register_conflict_returns_existing() {
        let dir = InMemoryDirectory::new();
        let addr1 = alice_address(1);
        let addr2 = ActorAddress::new(alice_id(), test_endpoint_at(4501), ActivationId::new(2));

        let first = dir.register(addr1.clone()).await.expect("first register");
        assert_eq!(first, addr1);

        // Second register should return the existing entry, not overwrite
        let second = dir.register(addr2).await.expect("second register");
        assert_eq!(second, addr1);
        assert_eq!(second.activation_id, ActivationId::new(1));
    }

    #[tokio::test]
    async fn test_unregister_matching_activation() {
        let dir = InMemoryDirectory::new();
        let addr = alice_address(1);

        dir.register(addr.clone()).await.expect("register");
        dir.unregister(&addr).await.expect("unregister");

        let result = dir.lookup(&alice_id()).await.expect("lookup");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_unregister_mismatched_activation() {
        let dir = InMemoryDirectory::new();
        let addr1 = alice_address(1);
        let addr2 = alice_address(2);

        dir.register(addr1.clone()).await.expect("register");

        // Unregister with wrong activation ID should do nothing
        dir.unregister(&addr2)
            .await
            .expect("unregister should succeed");

        // Original entry should still be present
        let result = dir.lookup(&alice_id()).await.expect("lookup");
        assert_eq!(result, Some(addr1));
    }

    #[tokio::test]
    async fn test_unregister_not_found_is_ok() {
        let dir = InMemoryDirectory::new();
        let addr = alice_address(1);

        // Unregistering an unknown actor should succeed silently
        let result = dir.unregister(&addr).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unregister_members() {
        let dir = InMemoryDirectory::new();
        let ep_a = test_endpoint_at(4500);
        let ep_b = test_endpoint_at(4501);

        let alice = ActorAddress::new(alice_id(), ep_a.clone(), ActivationId::new(1));
        let bob = ActorAddress::new(bob_id(), ep_b.clone(), ActivationId::new(2));

        dir.register(alice).await.expect("register alice");
        dir.register(bob.clone()).await.expect("register bob");

        // Remove all actors on port 4500
        let addr_a = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let removed = dir
            .unregister_members(&[addr_a])
            .await
            .expect("unregister_members");

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].actor_id, alice_id());

        // Alice should be gone, bob should remain
        assert!(dir.lookup(&alice_id()).await.expect("lookup").is_none());
        assert_eq!(dir.lookup(&bob_id()).await.expect("lookup"), Some(bob));
    }

    #[tokio::test]
    async fn test_multiple_actors() {
        let dir = InMemoryDirectory::new();
        let alice = alice_address(1);
        let bob = bob_address(2);

        dir.register(alice.clone()).await.expect("register alice");
        dir.register(bob.clone()).await.expect("register bob");

        assert_eq!(
            dir.lookup(&alice_id()).await.expect("lookup alice"),
            Some(alice)
        );
        assert_eq!(dir.lookup(&bob_id()).await.expect("lookup bob"), Some(bob));
    }
}
