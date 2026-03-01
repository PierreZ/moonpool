//! Placement strategy: decides where to activate a new actor.
//!
//! When the directory has no entry for an actor, the placement strategy
//! chooses which node should host it. The simplest strategy is "always
//! local" — every actor is placed on the node that first calls it.
//!
//! # Design
//!
//! - `PlacementStrategy` is a trait so implementations can range from
//!   local-only to hash-based or load-balanced placement.
//! - The strategy receives the `ActorId` and a list of active member
//!   addresses from the membership provider, then returns the chosen endpoint.
//!
//! # Orleans Reference
//!
//! This corresponds to Orleans' placement directors (RandomPlacement,
//! PreferLocalPlacement, HashBasedPlacement).

use std::fmt;

use crate::{Endpoint, NetworkAddress, UID};

use super::ActorId;

/// Errors from placement operations.
#[derive(Debug, thiserror::Error)]
pub enum PlacementError {
    /// No candidate endpoints available for placement.
    #[error("no candidates available for actor {id:?}")]
    NoCandidates {
        /// The actor that could not be placed.
        id: ActorId,
    },
}

/// Strategy for deciding where to place a new actor instance.
///
/// Called by the `ActorRouter` when an actor is not found in the directory.
/// The `active_members` parameter is populated from the membership provider's
/// list of currently active nodes.
#[async_trait::async_trait(?Send)]
pub trait PlacementStrategy: fmt::Debug {
    /// Choose an endpoint for the given actor from the active members.
    ///
    /// # Arguments
    ///
    /// * `id` - The actor to place
    /// * `active_members` - Network addresses of all active cluster members
    async fn place(
        &self,
        id: &ActorId,
        active_members: &[NetworkAddress],
    ) -> Result<Endpoint, PlacementError>;
}

/// Always place on the local node.
///
/// The simplest placement strategy — every actor is activated on the
/// node that first sends it a message. Suitable for single-node systems.
#[derive(Debug)]
pub struct LocalPlacement {
    /// The local node's network address.
    local_address: NetworkAddress,
}

impl LocalPlacement {
    /// Create a new local placement strategy.
    pub fn new(local_address: NetworkAddress) -> Self {
        Self { local_address }
    }
}

#[async_trait::async_trait(?Send)]
impl PlacementStrategy for LocalPlacement {
    async fn place(
        &self,
        id: &ActorId,
        _active_members: &[NetworkAddress],
    ) -> Result<Endpoint, PlacementError> {
        // Actor type's dispatch token is always index 0
        Ok(Endpoint::new(
            self.local_address.clone(),
            UID::new(id.actor_type.0, 0),
        ))
    }
}

/// Round-robin placement across active cluster members.
///
/// Distributes actors evenly across the active members. Each call
/// to `place()` picks the next member in rotation.
///
/// # Example
///
/// ```rust,ignore
/// let placement = RoundRobinPlacement::new();
/// // With 3 active members: first → A, second → B, third → C, fourth → A, ...
/// ```
#[derive(Debug)]
pub struct RoundRobinPlacement {
    /// Next index to use (wraps around).
    next: std::cell::Cell<usize>,
}

impl RoundRobinPlacement {
    /// Create a new round-robin placement strategy.
    pub fn new() -> Self {
        Self {
            next: std::cell::Cell::new(0),
        }
    }
}

impl Default for RoundRobinPlacement {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait(?Send)]
impl PlacementStrategy for RoundRobinPlacement {
    async fn place(
        &self,
        id: &ActorId,
        active_members: &[NetworkAddress],
    ) -> Result<Endpoint, PlacementError> {
        if active_members.is_empty() {
            return Err(PlacementError::NoCandidates { id: id.clone() });
        }
        let index = self.next.get() % active_members.len();
        self.next.set(index + 1);
        let address = active_members[index].clone();
        // Actor type's dispatch token is always index 0
        Ok(Endpoint::new(address, UID::new(id.actor_type.0, 0)))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::NetworkAddress;

    use super::*;
    use crate::actors::ActorType;

    fn addr(port: u16) -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn alice_id() -> ActorId {
        ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "alice".to_string(),
        }
    }

    #[tokio::test]
    async fn test_local_placement_always_returns_local() {
        let local_addr = addr(4500);
        let strategy = LocalPlacement::new(local_addr.clone());

        let result = strategy
            .place(&alice_id(), &[addr(4500), addr(4501)])
            .await
            .expect("placement should succeed");

        assert_eq!(result.address, local_addr);
    }

    #[tokio::test]
    async fn test_local_placement_ignores_active_members() {
        let local_addr = addr(4500);
        let strategy = LocalPlacement::new(local_addr.clone());

        let result = strategy
            .place(&alice_id(), &[addr(4501), addr(4502)])
            .await
            .expect("placement should succeed");

        // Should always return local address, regardless of active_members
        assert_eq!(result.address, local_addr);
    }

    #[tokio::test]
    async fn test_round_robin_distributes_across_members() {
        let strategy = RoundRobinPlacement::new();
        let members = vec![addr(4500), addr(4501), addr(4502)];

        let r1 = strategy.place(&alice_id(), &members).await.expect("place");
        let r2 = strategy.place(&alice_id(), &members).await.expect("place");
        let r3 = strategy.place(&alice_id(), &members).await.expect("place");
        let r4 = strategy.place(&alice_id(), &members).await.expect("place");

        assert_eq!(r1.address, addr(4500));
        assert_eq!(r2.address, addr(4501));
        assert_eq!(r3.address, addr(4502));
        assert_eq!(r4.address, addr(4500)); // wraps around
    }

    #[tokio::test]
    async fn test_round_robin_no_candidates_errors() {
        let strategy = RoundRobinPlacement::new();

        let result = strategy.place(&alice_id(), &[]).await;
        assert!(matches!(result, Err(PlacementError::NoCandidates { .. })));
    }
}
