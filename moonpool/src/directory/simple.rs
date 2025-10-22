//! Simple directory implementation with eventual consistency.
//!
//! This module provides a basic in-memory directory for actor location tracking
//! in a single-threaded environment.

use crate::actor::{ActorId, NodeId};
use crate::directory::{Directory, PlacementDecision};
use crate::error::DirectoryError;
use async_trait::async_trait;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Simple directory implementation with local caching.
///
/// `SimpleDirectory` provides a basic actor location directory with:
/// - Local in-memory location map (ActorId → NodeId)
/// - Eventual consistency model
/// - Two-random-choices placement algorithm
/// - Race detection for concurrent activations
///
/// # Architecture
///
/// ```text
/// ┌─────────────────────────────────────┐
/// │ SimpleDirectory (per node)          │
/// │                                     │
/// │  ┌───────────────────────────────┐  │
/// │  │ location_map: ActorId → NodeId│  │
/// │  └───────────────────────────────┘  │
/// │                                     │
/// │  ┌───────────────────────────────┐  │
/// │  │ node_load: NodeId → usize    │  │
/// │  └───────────────────────────────┘  │
/// │                                     │
/// │  ┌───────────────────────────────┐  │
/// │  │ cluster_nodes: Vec<NodeId>   │  │
/// │  └───────────────────────────────┘  │
/// └─────────────────────────────────────┘
/// ```
///
/// # Consistency Model
///
/// **Read-your-writes**: Local writes immediately visible to local reads.
/// **Eventual consistency**: Remote nodes may have stale cache entries.
/// **Cache invalidation**: Piggyback updates on message responses.
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::prelude::*;
/// use moonpool::directory::SimpleDirectory;
///
/// // Create directory with cluster nodes
/// let nodes = vec![
///     NodeId::parse("node1:8001")?,
///     NodeId::parse("node2:8002")?,
///     NodeId::parse("node3:8003")?,
/// ];
/// let directory = SimpleDirectory::new(nodes.clone());
///
/// // Register actor
/// let actor_id = ActorId::parse("prod::BankAccount/alice")?;
/// let my_node = nodes[0].clone();
///
/// match directory.register(actor_id.clone(), my_node.clone()).await? {
///     PlacementDecision::PlaceOnNode(node) => {
///         println!("Actor placed on: {}", node);
///     }
///     PlacementDecision::AlreadyRegistered(node) => {
///         println!("Actor already exists on: {}", node);
///     }
///     PlacementDecision::Race { winner, loser } => {
///         println!("Race detected: winner={}, loser={}", winner, loser);
///     }
/// }
///
/// // Lookup actor location
/// if let Some(node) = directory.lookup(&actor_id).await? {
///     println!("Actor located on: {}", node);
/// }
/// ```
#[derive(Clone)]
pub struct SimpleDirectory {
    /// Shared state (using Rc<RefCell> for single-threaded interior mutability)
    state: Rc<RefCell<DirectoryState>>,
}

/// Internal state for SimpleDirectory.
struct DirectoryState {
    /// Location map: ActorId → NodeId
    ///
    /// Tracks where each actor is currently activated.
    /// Updated on register/unregister operations.
    location_map: HashMap<String, NodeId>,

    /// Node load tracking: NodeId → actor count
    ///
    /// Used by two-random-choices placement algorithm.
    /// Incremented on register, decremented on unregister.
    node_load: HashMap<NodeId, usize>,

    /// List of all nodes in the cluster.
    ///
    /// Used for placement decisions. Should be updated when nodes join/leave.
    cluster_nodes: Vec<NodeId>,
}

impl SimpleDirectory {
    /// Create a new SimpleDirectory with specified cluster nodes.
    ///
    /// # Parameters
    ///
    /// - `cluster_nodes`: List of all nodes in the cluster
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let nodes = vec![
    ///     NodeId::parse("127.0.0.1:8001")?,
    ///     NodeId::parse("127.0.0.1:8002")?,
    ///     NodeId::parse("127.0.0.1:8003")?,
    /// ];
    /// let directory = SimpleDirectory::new(nodes);
    /// ```
    pub fn new(cluster_nodes: Vec<NodeId>) -> Self {
        let mut node_load = HashMap::new();
        for node in &cluster_nodes {
            node_load.insert(node.clone(), 0);
        }

        Self {
            state: Rc::new(RefCell::new(DirectoryState {
                location_map: HashMap::new(),
                node_load,
                cluster_nodes,
            })),
        }
    }

    /// Create storage key for an ActorId.
    ///
    /// Format: `namespace::actor_type/key`
    ///
    /// This provides natural isolation:
    /// - Different namespaces can't access each other's actors
    /// - Different actor types are isolated
    fn storage_key(actor_id: &ActorId) -> String {
        format!(
            "{}::{}/{}",
            actor_id.namespace, actor_id.actor_type, actor_id.key
        )
    }

    /// Get current load for a node.
    ///
    /// Returns 0 if node not found (defensive).
    #[allow(dead_code)]
    fn get_node_load(&self, node_id: &NodeId) -> usize {
        self.state
            .borrow()
            .node_load
            .get(node_id)
            .copied()
            .unwrap_or(0)
    }

    /// Increment node load counter.
    fn increment_node_load(&self, node_id: &NodeId) {
        let mut state = self.state.borrow_mut();
        *state.node_load.entry(node_id.clone()).or_insert(0) += 1;
    }

    /// Decrement node load counter.
    fn decrement_node_load(&self, node_id: &NodeId) {
        let mut state = self.state.borrow_mut();
        if let Some(count) = state.node_load.get_mut(node_id) {
            *count = count.saturating_sub(1);
        }
    }

    /// Two-random-choices placement algorithm.
    ///
    /// Algorithm:
    /// 1. Pick two random nodes from cluster
    /// 2. Return the less-loaded one
    ///
    /// This provides near-optimal load balancing with minimal overhead.
    /// See research.md for analysis.
    fn choose_placement_node_internal(&self) -> Result<NodeId, DirectoryError> {
        let state = self.state.borrow();

        if state.cluster_nodes.is_empty() {
            return Err(DirectoryError::Unavailable);
        }

        if state.cluster_nodes.len() == 1 {
            return Ok(state.cluster_nodes[0].clone());
        }

        // Two-random-choices algorithm
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();

        // Pick two distinct random nodes
        let candidates: Vec<_> = state.cluster_nodes.choose_multiple(&mut rng, 2).collect();

        if candidates.len() < 2 {
            // Fallback: just pick the first node
            return Ok(state.cluster_nodes[0].clone());
        }

        let node1 = candidates[0];
        let node2 = candidates[1];

        let load1 = state.node_load.get(node1).copied().unwrap_or(0);
        let load2 = state.node_load.get(node2).copied().unwrap_or(0);

        // Choose less-loaded node
        if load1 <= load2 {
            Ok(node1.clone())
        } else {
            Ok(node2.clone())
        }
    }
}

#[async_trait(?Send)]
impl Directory for SimpleDirectory {
    async fn lookup(&self, actor_id: &ActorId) -> Result<Option<NodeId>, DirectoryError> {
        let key = Self::storage_key(actor_id);
        let state = self.state.borrow();

        Ok(state.location_map.get(&key).cloned())
    }

    async fn register(
        &self,
        actor_id: ActorId,
        node_id: NodeId,
    ) -> Result<PlacementDecision, DirectoryError> {
        let key = Self::storage_key(&actor_id);

        // Critical section: check and potentially update location map
        let mut state = self.state.borrow_mut();

        match state.location_map.get(&key) {
            None => {
                // No existing registration - place on requesting node
                state.location_map.insert(key, node_id.clone());
                drop(state); // Release borrow before calling increment

                self.increment_node_load(&node_id);

                Ok(PlacementDecision::PlaceOnNode(node_id))
            }
            Some(existing_node) if existing_node == &node_id => {
                // Already registered on same node (idempotent)
                Ok(PlacementDecision::PlaceOnNode(node_id))
            }
            Some(existing_node) => {
                // Already registered on different node
                let existing_node = existing_node.clone();

                // Race detection: If this is a different node trying to register,
                // we have a concurrent activation race
                if existing_node != node_id {
                    // Race detected: existing node wins (first-wins strategy)
                    Ok(PlacementDecision::Race {
                        winner: existing_node.clone(),
                        loser: node_id,
                    })
                } else {
                    Ok(PlacementDecision::AlreadyRegistered(existing_node))
                }
            }
        }
    }

    async fn unregister(&self, actor_id: &ActorId) -> Result<(), DirectoryError> {
        let key = Self::storage_key(actor_id);
        let mut state = self.state.borrow_mut();

        if let Some(node_id) = state.location_map.remove(&key) {
            drop(state); // Release borrow before calling decrement
            self.decrement_node_load(&node_id);
        }

        // Idempotent: unregistering non-existent actor succeeds
        Ok(())
    }

    async fn choose_placement_node(&self) -> Result<NodeId, DirectoryError> {
        self.choose_placement_node_internal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simple_directory_basic_operations() {
        let nodes = vec![
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];
        let directory = SimpleDirectory::new(nodes.clone());

        let actor_id = ActorId::from_string("test::Counter/alice").unwrap();

        // Lookup non-existent actor
        assert_eq!(directory.lookup(&actor_id).await.unwrap(), None);

        // Register actor
        let decision = directory
            .register(actor_id.clone(), nodes[0].clone())
            .await
            .unwrap();
        assert!(decision.is_successful());

        // Lookup registered actor
        let location = directory.lookup(&actor_id).await.unwrap();
        assert_eq!(location, Some(nodes[0].clone()));

        // Unregister actor
        directory.unregister(&actor_id).await.unwrap();

        // Lookup unregistered actor
        assert_eq!(directory.lookup(&actor_id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_concurrent_registration_race() {
        let nodes = vec![
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];
        let directory = SimpleDirectory::new(nodes.clone());

        let actor_id = ActorId::from_string("test::Counter/bob").unwrap();

        // Node 1 registers first
        let decision1 = directory
            .register(actor_id.clone(), nodes[0].clone())
            .await
            .unwrap();
        assert!(matches!(decision1, PlacementDecision::PlaceOnNode(_)));

        // Node 2 tries to register same actor (race condition)
        let decision2 = directory
            .register(actor_id.clone(), nodes[1].clone())
            .await
            .unwrap();

        // Should detect race
        assert!(matches!(decision2, PlacementDecision::Race { .. }));

        if let PlacementDecision::Race { winner, loser } = decision2 {
            assert_eq!(winner, nodes[0]);
            assert_eq!(loser, nodes[1]);
        }
    }

    #[tokio::test]
    async fn test_node_load_tracking() {
        let nodes = vec![
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];
        let directory = SimpleDirectory::new(nodes.clone());

        // Initially zero load
        assert_eq!(directory.get_node_load(&nodes[0]), 0);
        assert_eq!(directory.get_node_load(&nodes[1]), 0);

        // Register actor on node 0
        let actor1 = ActorId::from_string("test::Counter/a1").unwrap();
        directory
            .register(actor1.clone(), nodes[0].clone())
            .await
            .unwrap();

        assert_eq!(directory.get_node_load(&nodes[0]), 1);
        assert_eq!(directory.get_node_load(&nodes[1]), 0);

        // Register another actor on node 0
        let actor2 = ActorId::from_string("test::Counter/a2").unwrap();
        directory
            .register(actor2.clone(), nodes[0].clone())
            .await
            .unwrap();

        assert_eq!(directory.get_node_load(&nodes[0]), 2);

        // Unregister one actor
        directory.unregister(&actor1).await.unwrap();

        assert_eq!(directory.get_node_load(&nodes[0]), 1);

        // Unregister second actor
        directory.unregister(&actor2).await.unwrap();

        assert_eq!(directory.get_node_load(&nodes[0]), 0);
    }

    #[tokio::test]
    async fn test_idempotent_operations() {
        let nodes = vec![NodeId::from("127.0.0.1:8001").unwrap()];
        let directory = SimpleDirectory::new(nodes.clone());

        let actor_id = ActorId::from_string("test::Counter/charlie").unwrap();

        // Register actor
        directory
            .register(actor_id.clone(), nodes[0].clone())
            .await
            .unwrap();

        // Register again (idempotent)
        let decision = directory
            .register(actor_id.clone(), nodes[0].clone())
            .await
            .unwrap();
        assert!(decision.is_successful());

        // Unregister
        directory.unregister(&actor_id).await.unwrap();

        // Unregister again (idempotent)
        directory.unregister(&actor_id).await.unwrap();

        // Should still succeed
        assert_eq!(directory.lookup(&actor_id).await.unwrap(), None);
    }
}
