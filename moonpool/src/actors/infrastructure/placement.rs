//! Placement: per-actor-type hints and cluster-level director.
//!
//! Placement separates two concerns (following the Orleans model):
//!
//! - **`PlacementStrategy`** — a lightweight enum declared per actor type,
//!   saying *what* the actor wants (e.g. local-only vs round-robin).
//! - **`PlacementDirector`** — a cluster-level trait that interprets the
//!   strategy, receiving context (actor ID, active members, local address)
//!   and producing a concrete `Endpoint`.
//!
//! # Orleans Reference
//!
//! This corresponds to Orleans' `PlacementStrategy` attribute (data on the
//! grain class) + `IPlacementDirector` (stateful algorithm registered in
//! the silo).

use std::cell::Cell;
use std::fmt;

use crate::actors::ActorId;
use crate::{Endpoint, NetworkAddress, UID};

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

/// Per-actor-type placement hint.
///
/// Declared on [`ActorHandler::placement_strategy()`] to tell the
/// [`PlacementDirector`] what kind of placement this actor type wants.
/// Lightweight (`Copy`) — no infrastructure state.
///
/// [`ActorHandler::placement_strategy()`]: crate::actors::ActorHandler::placement_strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlacementStrategy {
    /// Place on the local node (the node that first sends the message).
    #[default]
    Local,
    /// Distribute across active cluster members in round-robin order.
    RoundRobin,
}

/// Cluster-level algorithm that interprets a [`PlacementStrategy`] hint.
///
/// Lives on [`ClusterConfig`], shared across all nodes. Receives the hint
/// plus context and produces an [`Endpoint`].
///
/// [`ClusterConfig`]: crate::actors::ClusterConfig
#[async_trait::async_trait(?Send)]
pub trait PlacementDirector: fmt::Debug {
    /// Choose an endpoint for the given actor.
    ///
    /// # Arguments
    ///
    /// * `strategy` - The per-actor-type placement hint
    /// * `id` - The actor to place
    /// * `active_members` - Network addresses of all active cluster members
    /// * `local_address` - This node's own address
    async fn place(
        &self,
        strategy: PlacementStrategy,
        id: &ActorId,
        active_members: &[NetworkAddress],
        local_address: &NetworkAddress,
    ) -> Result<Endpoint, PlacementError>;
}

/// Built-in placement director handling `Local` and `RoundRobin` strategies.
#[derive(Debug, Default)]
pub struct DefaultPlacementDirector {
    /// Next index for round-robin (wraps around).
    round_robin_next: Cell<usize>,
}

#[async_trait::async_trait(?Send)]
impl PlacementDirector for DefaultPlacementDirector {
    async fn place(
        &self,
        strategy: PlacementStrategy,
        id: &ActorId,
        active_members: &[NetworkAddress],
        local_address: &NetworkAddress,
    ) -> Result<Endpoint, PlacementError> {
        match strategy {
            PlacementStrategy::Local => Ok(Endpoint::new(
                local_address.clone(),
                UID::new(id.actor_type.0, 0),
            )),
            PlacementStrategy::RoundRobin => {
                if active_members.is_empty() {
                    return Err(PlacementError::NoCandidates { id: id.clone() });
                }
                let index = self.round_robin_next.get() % active_members.len();
                self.round_robin_next.set(index + 1);
                let address = active_members[index].clone();
                Ok(Endpoint::new(address, UID::new(id.actor_type.0, 0)))
            }
        }
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
    async fn test_local_placement_returns_local_address() {
        let local = addr(4500);
        let director = DefaultPlacementDirector::default();

        let result = director
            .place(
                PlacementStrategy::Local,
                &alice_id(),
                &[addr(4500), addr(4501)],
                &local,
            )
            .await
            .expect("placement should succeed");

        assert_eq!(result.address, local);
    }

    #[tokio::test]
    async fn test_local_placement_ignores_active_members() {
        let local = addr(4500);
        let director = DefaultPlacementDirector::default();

        let result = director
            .place(
                PlacementStrategy::Local,
                &alice_id(),
                &[addr(4501), addr(4502)],
                &local,
            )
            .await
            .expect("placement should succeed");

        assert_eq!(result.address, local);
    }

    #[tokio::test]
    async fn test_round_robin_distributes_across_members() {
        let local = addr(4500);
        let director = DefaultPlacementDirector::default();
        let members = vec![addr(4500), addr(4501), addr(4502)];

        let r1 = director
            .place(PlacementStrategy::RoundRobin, &alice_id(), &members, &local)
            .await
            .expect("place");
        let r2 = director
            .place(PlacementStrategy::RoundRobin, &alice_id(), &members, &local)
            .await
            .expect("place");
        let r3 = director
            .place(PlacementStrategy::RoundRobin, &alice_id(), &members, &local)
            .await
            .expect("place");
        let r4 = director
            .place(PlacementStrategy::RoundRobin, &alice_id(), &members, &local)
            .await
            .expect("place");

        assert_eq!(r1.address, addr(4500));
        assert_eq!(r2.address, addr(4501));
        assert_eq!(r3.address, addr(4502));
        assert_eq!(r4.address, addr(4500)); // wraps around
    }

    #[tokio::test]
    async fn test_round_robin_no_candidates_errors() {
        let local = addr(4500);
        let director = DefaultPlacementDirector::default();

        let result = director
            .place(PlacementStrategy::RoundRobin, &alice_id(), &[], &local)
            .await;
        assert!(matches!(result, Err(PlacementError::NoCandidates { .. })));
    }

    #[test]
    fn test_placement_strategy_default_is_local() {
        assert_eq!(PlacementStrategy::default(), PlacementStrategy::Local);
    }
}
