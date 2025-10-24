//! Simple placement strategy implementation.
//!
//! This module provides basic placement algorithms for choosing which node
//! should host a new actor activation.

use crate::actor::{ActorId, NodeId, PlacementHint};
use crate::error::DirectoryError;
use std::collections::HashMap;

/// Simple placement strategy for actor node selection.
///
/// `SimplePlacement` implements basic placement algorithms:
/// - **Local**: Prefer the caller's node
/// - **Random**: Choose a random node (uniform distribution)
/// - **LeastLoaded**: Choose the node with fewest active actors
///
/// # Architecture
///
/// ```text
/// ┌─────────────────────────────────────┐
/// │ SimplePlacement                     │
/// │                                     │
/// │  ┌───────────────────────────────┐  │
/// │  │ available_nodes: Vec<NodeId> │  │
/// │  └───────────────────────────────┘  │
/// └─────────────────────────────────────┘
///          │
///          │ queries node_load from
///          ▼
/// ┌─────────────────────────────────────┐
/// │ Directory (location tracking)       │
/// │                                     │
/// │  ┌───────────────────────────────┐  │
/// │  │ node_load: NodeId → usize    │  │
/// │  └───────────────────────────────┘  │
/// └─────────────────────────────────────┘
/// ```
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::placement::SimplePlacement;
/// use moonpool::actor::{PlacementHint, NodeId};
///
/// // Create placement with available nodes
/// let nodes = vec![
///     NodeId::parse("node1:8001")?,
///     NodeId::parse("node2:8002")?,
///     NodeId::parse("node3:8003")?,
/// ];
/// let placement = SimplePlacement::new(nodes.clone());
///
/// // Choose node based on hint and current load
/// let node_loads = directory.get_all_node_loads();
/// let chosen_node = placement.choose_node(
///     &actor_id,
///     PlacementHint::LeastLoaded,
///     &nodes[0],  // caller node
///     &node_loads
/// )?;
/// ```
#[derive(Clone)]
pub struct SimplePlacement {
    /// List of all available nodes in the cluster.
    ///
    /// Used for placement decisions. Should be updated when nodes join/leave.
    available_nodes: Vec<NodeId>,
}

impl SimplePlacement {
    /// Create a new SimplePlacement with specified available nodes.
    ///
    /// # Parameters
    ///
    /// - `available_nodes`: List of all nodes available for placement
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let nodes = vec![
    ///     NodeId::parse("127.0.0.1:8001")?,
    ///     NodeId::parse("127.0.0.1:8002")?,
    ///     NodeId::parse("127.0.0.1:8003")?,
    /// ];
    /// let placement = SimplePlacement::new(nodes);
    /// ```
    pub fn new(available_nodes: Vec<NodeId>) -> Self {
        Self { available_nodes }
    }

    /// Choose a node for placing a new actor activation.
    ///
    /// # Parameters
    ///
    /// - `_actor_id`: The actor to place (reserved for future hash-based placement)
    /// - `hint`: Placement hint (Local, Random, or LeastLoaded)
    /// - `caller_node`: The node making the placement request
    /// - `node_loads`: Current load per node (from directory)
    ///
    /// # Returns
    ///
    /// - `Ok(node_id)`: Recommended node for activation
    /// - `Err(DirectoryError::Unavailable)`: No suitable node available
    ///
    /// # Placement Strategy
    ///
    /// - **PlacementHint::Local**: Return `caller_node` (prefer local activation)
    /// - **PlacementHint::Random**: Choose a random node (uniform distribution)
    /// - **PlacementHint::LeastLoaded**: Choose node with fewest active actors
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let node_loads = directory.get_all_node_loads();
    /// let chosen = placement.choose_node(
    ///     &actor_id,
    ///     PlacementHint::LeastLoaded,
    ///     &my_node,
    ///     &node_loads
    /// )?;
    /// directory.register(actor_id, chosen).await?;
    /// ```
    pub fn choose_node(
        &self,
        _actor_id: &ActorId,
        hint: PlacementHint,
        caller_node: &NodeId,
        node_loads: &HashMap<NodeId, usize>,
    ) -> Result<NodeId, DirectoryError> {
        match hint {
            PlacementHint::Local => {
                // Return caller's node if it's available
                if self.available_nodes.contains(caller_node) {
                    tracing::debug!(
                        caller_node = %caller_node,
                        "Placement: Honoring Local placement hint"
                    );
                    Ok(caller_node.clone())
                } else {
                    // Fallback to random if caller not available
                    tracing::warn!(
                        caller_node = %caller_node,
                        "Placement: Caller node not available, falling back to Random"
                    );
                    self.choose_random_node()
                }
            }
            PlacementHint::Random => {
                let node = self.choose_random_node()?;
                tracing::debug!(
                    chosen_node = %node,
                    "Placement: Random placement selected"
                );
                Ok(node)
            }
            PlacementHint::LeastLoaded => {
                let node = self.choose_least_loaded_node(node_loads)?;
                tracing::debug!(
                    chosen_node = %node,
                    "Placement: LeastLoaded placement selected"
                );
                Ok(node)
            }
        }
    }

    /// Choose a random node from available nodes.
    ///
    /// Uses uniform distribution.
    ///
    /// # Returns
    ///
    /// - `Ok(node_id)`: Randomly selected node
    /// - `Err(DirectoryError::Unavailable)`: No nodes available
    fn choose_random_node(&self) -> Result<NodeId, DirectoryError> {
        if self.available_nodes.is_empty() {
            return Err(DirectoryError::Unavailable);
        }

        use rand::prelude::IndexedRandom;
        let mut rng = rand::rng();

        self.available_nodes
            .choose(&mut rng)
            .cloned()
            .ok_or(DirectoryError::Unavailable)
    }

    /// Choose the least loaded node from available nodes.
    ///
    /// Selects the node with the minimum activation count (fewest active actors).
    /// Provides automatic load balancing based on current cluster state.
    ///
    /// # Parameters
    ///
    /// - `node_loads`: Current load per node (from directory)
    ///
    /// # Returns
    ///
    /// - `Ok(node_id)`: Node with lowest activation count
    /// - `Err(DirectoryError::Unavailable)`: No nodes available
    ///
    /// # Tie-Breaking
    ///
    /// If multiple nodes have the same minimum load, the first one encountered
    /// is returned (stable selection).
    fn choose_least_loaded_node(
        &self,
        node_loads: &HashMap<NodeId, usize>,
    ) -> Result<NodeId, DirectoryError> {
        if self.available_nodes.is_empty() {
            return Err(DirectoryError::Unavailable);
        }

        // Find node with minimum load among available nodes
        let least_loaded = self
            .available_nodes
            .iter()
            .map(|node| {
                let load = node_loads.get(node).copied().unwrap_or(0);
                (node.clone(), load)
            })
            .min_by_key(|(_, load)| *load);

        match least_loaded {
            Some((node, load)) => {
                tracing::debug!(
                    node_id = %node,
                    load = load,
                    "Placement: Selected least loaded node"
                );
                Ok(node)
            }
            None => {
                // Should not reach here if available_nodes is not empty
                tracing::error!("Placement: No nodes available despite non-empty available_nodes");
                Err(DirectoryError::Unavailable)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_placement() {
        let nodes = vec![
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];
        let placement = SimplePlacement::new(nodes.clone());
        let actor_id = ActorId::from_string("test::Counter/alice").unwrap();
        let node_loads = HashMap::new();

        let chosen = placement
            .choose_node(&actor_id, PlacementHint::Local, &nodes[0], &node_loads)
            .unwrap();

        assert_eq!(chosen, nodes[0]);
    }

    #[test]
    fn test_random_placement() {
        let nodes = vec![
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];
        let placement = SimplePlacement::new(nodes.clone());
        let actor_id = ActorId::from_string("test::Counter/alice").unwrap();
        let node_loads = HashMap::new();

        let chosen = placement
            .choose_node(&actor_id, PlacementHint::Random, &nodes[0], &node_loads)
            .unwrap();

        // Should be one of the available nodes
        assert!(nodes.contains(&chosen));
    }

    #[test]
    fn test_least_loaded_placement() {
        let nodes = vec![
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];
        let placement = SimplePlacement::new(nodes.clone());
        let actor_id = ActorId::from_string("test::Counter/alice").unwrap();

        let mut node_loads = HashMap::new();
        node_loads.insert(nodes[0].clone(), 10);
        node_loads.insert(nodes[1].clone(), 2);

        let chosen = placement
            .choose_node(
                &actor_id,
                PlacementHint::LeastLoaded,
                &nodes[0],
                &node_loads,
            )
            .unwrap();

        // Should choose node with load=2
        assert_eq!(chosen, nodes[1]);
    }

    #[test]
    fn test_unavailable_nodes() {
        let placement = SimplePlacement::new(vec![]);
        let actor_id = ActorId::from_string("test::Counter/alice").unwrap();
        let caller = NodeId::from("127.0.0.1:8001").unwrap();
        let node_loads = HashMap::new();

        let result = placement.choose_node(&actor_id, PlacementHint::Random, &caller, &node_loads);

        assert!(matches!(result, Err(DirectoryError::Unavailable)));
    }
}
