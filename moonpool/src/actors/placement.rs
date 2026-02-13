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
//! - The strategy receives the `ActorId` and a list of candidate endpoints,
//!   then returns the chosen endpoint.
//!
//! # Orleans Reference
//!
//! This corresponds to Orleans' placement directors (RandomPlacement,
//! PreferLocalPlacement, HashBasedPlacement).

use std::fmt;

use crate::Endpoint;

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
#[async_trait::async_trait(?Send)]
pub trait PlacementStrategy: fmt::Debug {
    /// Choose an endpoint from the candidates for the given actor.
    ///
    /// # Arguments
    ///
    /// * `id` - The actor to place
    /// * `candidates` - Available endpoints (typically all known nodes)
    async fn place(
        &self,
        id: &ActorId,
        candidates: &[Endpoint],
    ) -> Result<Endpoint, PlacementError>;
}

/// Always place on the local node.
///
/// The simplest placement strategy — every actor is activated on the
/// node that first sends it a message. Suitable for single-node systems.
#[derive(Debug)]
pub struct LocalPlacement {
    /// The local node's endpoint for actor dispatch.
    pub local_endpoint: Endpoint,
}

impl LocalPlacement {
    /// Create a new local placement strategy.
    pub fn new(local_endpoint: Endpoint) -> Self {
        Self { local_endpoint }
    }
}

#[async_trait::async_trait(?Send)]
impl PlacementStrategy for LocalPlacement {
    async fn place(
        &self,
        _id: &ActorId,
        _candidates: &[Endpoint],
    ) -> Result<Endpoint, PlacementError> {
        Ok(self.local_endpoint.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::{NetworkAddress, UID};

    use super::*;
    use crate::actors::ActorType;

    fn local_endpoint() -> Endpoint {
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

    #[tokio::test]
    async fn test_local_placement_always_returns_local() {
        let endpoint = local_endpoint();
        let strategy = LocalPlacement::new(endpoint.clone());

        let result = strategy
            .place(&alice_id(), &[])
            .await
            .expect("placement should succeed");

        assert_eq!(result, endpoint);
    }

    #[tokio::test]
    async fn test_local_placement_ignores_candidates() {
        let local = local_endpoint();
        let strategy = LocalPlacement::new(local.clone());

        let remote = Endpoint::new(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4501),
            UID::new(0xBA4E_4B00, 0),
        );

        let result = strategy
            .place(&alice_id(), &[remote])
            .await
            .expect("placement should succeed");

        assert_eq!(result, local);
    }
}
