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

/// Simple directory implementation for location tracking.
///
/// `SimpleDirectory` provides a basic actor location directory with:
/// - Local in-memory location map (ActorId → NodeId)
/// - Node load tracking (NodeId → actor count)
/// - Race detection for concurrent activations
/// - Eventual consistency model
///
/// # Architecture
///
/// ```text
/// ┌─────────────────────────────────────┐
/// │ SimpleDirectory                     │
/// │                                     │
/// │  ┌───────────────────────────────┐  │
/// │  │ location_map: ActorId → NodeId│  │  (WHERE actors are)
/// │  └───────────────────────────────┘  │
/// │                                     │
/// │  ┌───────────────────────────────┐  │
/// │  │ node_load: NodeId → usize    │  │  (HOW MANY actors per node)
/// │  └───────────────────────────────┘  │
/// └─────────────────────────────────────┘
/// ```
///
/// # Separation of Concerns
///
/// **Directory** (this module): Tracks WHERE actors are currently located
/// **Placement** (placement module): Decides WHERE new actors should go
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
/// use moonpool::placement::SimplePlacement;
///
/// // Create directory (tracks locations)
/// let directory = SimpleDirectory::new();
///
/// // Create placement (chooses nodes)
/// let nodes = vec![
///     NodeId::parse("node1:8001")?,
///     NodeId::parse("node2:8002")?,
///     NodeId::parse("node3:8003")?,
/// ];
/// let placement = SimplePlacement::new(nodes.clone());
///
/// // Place a new actor
/// let actor_id = ActorId::parse("prod::BankAccount/alice")?;
/// let node_loads = directory.get_all_node_loads().await;
/// let chosen_node = placement.choose_node(&actor_id, PlacementHint::LeastLoaded, &nodes[0], &node_loads)?;
///
/// // Register actor at chosen node
/// match directory.register(actor_id.clone(), chosen_node).await? {
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
    /// Tracks how many actors are registered on each node.
    /// Incremented on register, decremented on unregister.
    /// Exposed to Placement module for load-aware placement decisions.
    node_load: HashMap<NodeId, usize>,
}

impl SimpleDirectory {
    /// Create a new SimpleDirectory.
    ///
    /// The directory starts empty and tracks actors as they are registered.
    /// Node load counters are created automatically as actors are registered.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let directory = SimpleDirectory::new();
    /// ```
    pub fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(DirectoryState {
                location_map: HashMap::new(),
                node_load: HashMap::new(),
            })),
        }
    }
}

impl Default for SimpleDirectory {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleDirectory {
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

    /// Increment node load counter.
    fn increment_node_load(&self, node_id: &NodeId) {
        let mut state = self.state.borrow_mut();
        *state.node_load.entry(node_id.clone()).or_insert(0) += 1;

        // Log current load distribution
        let load_summary: Vec<_> = state
            .node_load
            .iter()
            .map(|(node, load)| format!("{}={}", node, load))
            .collect();
        tracing::debug!("Directory load: [{}]", load_summary.join(", "));
    }

    /// Decrement node load counter.
    fn decrement_node_load(&self, node_id: &NodeId) {
        let mut state = self.state.borrow_mut();
        if let Some(count) = state.node_load.get_mut(node_id) {
            *count = count.saturating_sub(1);
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
                state.location_map.insert(key.clone(), node_id.clone());
                drop(state); // Release borrow before calling increment

                self.increment_node_load(&node_id);

                tracing::info!(
                    actor_id = %actor_id,
                    node_id = %node_id,
                    "Directory: Placed actor on node"
                );

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

    async fn get_actor_count(&self, node_id: &NodeId) -> usize {
        let state = self.state.borrow();
        state.node_load.get(node_id).copied().unwrap_or(0)
    }

    async fn get_all_node_loads(&self) -> HashMap<NodeId, usize> {
        let state = self.state.borrow();
        state.node_load.clone()
    }

    async fn get_actors_on_node(&self, node_id: &NodeId) -> Vec<ActorId> {
        let state = self.state.borrow();

        // Iterate through location_map and collect actors on the specified node
        state
            .location_map
            .iter()
            .filter_map(|(key, location)| {
                if location == node_id {
                    // Parse the storage key back to ActorId
                    // Format: "namespace::actor_type/key"
                    ActorId::from_string(key).ok()
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_simple_directory_basic_operations() {
        let directory = SimpleDirectory::new();

        let nodes = [
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];

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
        let directory = SimpleDirectory::new();

        let nodes = [
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];

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
        let directory = SimpleDirectory::new();

        let nodes = [
            NodeId::from("127.0.0.1:8001").unwrap(),
            NodeId::from("127.0.0.1:8002").unwrap(),
        ];

        // Initially zero load
        assert_eq!(directory.get_actor_count(&nodes[0]).await, 0);
        assert_eq!(directory.get_actor_count(&nodes[1]).await, 0);

        // Register actor on node 0
        let actor1 = ActorId::from_string("test::Counter/a1").unwrap();
        directory
            .register(actor1.clone(), nodes[0].clone())
            .await
            .unwrap();

        assert_eq!(directory.get_actor_count(&nodes[0]).await, 1);
        assert_eq!(directory.get_actor_count(&nodes[1]).await, 0);

        // Register another actor on node 0
        let actor2 = ActorId::from_string("test::Counter/a2").unwrap();
        directory
            .register(actor2.clone(), nodes[0].clone())
            .await
            .unwrap();

        assert_eq!(directory.get_actor_count(&nodes[0]).await, 2);

        // Unregister one actor
        directory.unregister(&actor1).await.unwrap();

        assert_eq!(directory.get_actor_count(&nodes[0]).await, 1);

        // Unregister second actor
        directory.unregister(&actor2).await.unwrap();

        assert_eq!(directory.get_actor_count(&nodes[0]).await, 0);
    }

    #[tokio::test]
    async fn test_idempotent_operations() {
        let directory = SimpleDirectory::new();

        let nodes = [NodeId::from("127.0.0.1:8001").unwrap()];

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
