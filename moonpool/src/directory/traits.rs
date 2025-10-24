//! Directory trait definitions.
//!
//! The Directory is responsible for tracking actor locations across the cluster.

use crate::actor::{ActorId, NodeId};
use crate::directory::PlacementDecision;
use crate::error::DirectoryError;
use async_trait::async_trait;

/// Directory service for actor location tracking.
///
/// The Directory maps `ActorId` to `NodeId`, enabling location-transparent
/// messaging. It provides eventual consistency with cache invalidation support.
///
/// # Responsibilities
///
/// 1. **Location Tracking**: Map ActorId → NodeId for routing
/// 2. **Race Detection**: Handle concurrent activation attempts
/// 3. **Load Tracking**: Maintain actor count per node
///
/// # Note on Placement
///
/// The Directory does NOT choose where actors should be placed. That responsibility
/// belongs to the Placement module. The Directory only tracks where actors ARE.
///
/// # Consistency Model
///
/// **Eventual Consistency**: Nodes may temporarily have stale location information.
/// Cache invalidation messages piggyback on responses to update stale caches.
///
/// # Concurrency
///
/// - **Single activation guarantee**: At most one activation per ActorId globally
/// - **Race handling**: Concurrent registrations resolved via first-wins strategy
/// - **Read-your-writes**: Local writes immediately visible to local reads
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::prelude::*;
///
/// let directory = SimpleDirectory::new(vec![node1, node2, node3]);
///
/// // Register actor on a node
/// match directory.register(actor_id.clone(), my_node_id.clone()).await? {
///     PlacementDecision::PlaceOnNode(node) => {
///         // We won the activation race, create actor
///         catalog.activate(actor_id).await?;
///     }
///     PlacementDecision::AlreadyRegistered(node) => {
///         // Actor exists elsewhere, forward message
///         forward_to(node, message).await?;
///     }
///     PlacementDecision::Race { winner, loser } => {
///         // Lost the race, deactivate our attempt
///         if loser == my_node_id {
///             catalog.deactivate(actor_id).await?;
///         }
///     }
/// }
///
/// // Lookup actor location
/// if let Some(node_id) = directory.lookup(&actor_id).await? {
///     send_message_to(node_id, message).await?;
/// }
/// ```
#[async_trait(?Send)]
pub trait Directory {
    /// Look up the current location of an actor.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The actor to locate
    ///
    /// # Returns
    ///
    /// - `Ok(Some(node_id))`: Actor is registered on the specified node
    /// - `Ok(None)`: Actor is not currently activated
    /// - `Err(DirectoryError)`: Directory operation failed
    ///
    /// # Caching
    ///
    /// Implementations may cache lookups locally. Stale cache entries are
    /// detected via "actor not found" errors and corrected via cache invalidation.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// match directory.lookup(&actor_id).await? {
    ///     Some(node_id) => {
    ///         // Route message to node_id
    ///         message_bus.send_to(node_id, message).await?;
    ///     }
    ///     None => {
    ///         // Actor not activated, trigger activation
    ///         let placement = directory.register(actor_id, my_node_id).await?;
    ///         // ... handle placement decision
    ///     }
    /// }
    /// ```
    async fn lookup(&self, actor_id: &ActorId) -> Result<Option<NodeId>, DirectoryError>;

    /// Register an actor at a specific node location.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The actor to register
    /// - `node_id`: The node where the actor is being activated
    ///
    /// # Returns
    ///
    /// - `PlacementDecision::PlaceOnNode(node)`: Registration successful, proceed with activation
    /// - `PlacementDecision::AlreadyRegistered(node)`: Actor already exists on another node
    /// - `PlacementDecision::Race { winner, loser }`: Concurrent activation detected
    ///
    /// # Concurrency
    ///
    /// Multiple nodes may attempt to register the same actor simultaneously.
    /// The directory serializes these requests and returns appropriate decisions:
    ///
    /// - **First registration**: Returns `PlaceOnNode`, activation proceeds
    /// - **Subsequent registrations**: Returns `AlreadyRegistered` or `Race`
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// match directory.register(actor_id.clone(), my_node_id.clone()).await? {
    ///     PlacementDecision::PlaceOnNode(node_id) => {
    ///         assert_eq!(node_id, my_node_id);
    ///         // Create activation
    ///         let context = ActorContext::new(actor_id, node_id, actor_instance);
    ///         catalog.record_activation(context).await?;
    ///     }
    ///     PlacementDecision::AlreadyRegistered(node_id) => {
    ///         // Forward message to existing activation
    ///         message_bus.forward_to(node_id, message).await?;
    ///     }
    ///     PlacementDecision::Race { winner, loser } => {
    ///         if loser == my_node_id {
    ///             // We lost, clean up partial activation
    ///             catalog.remove_activation(&actor_id).await?;
    ///         }
    ///     }
    /// }
    /// ```
    async fn register(
        &self,
        actor_id: ActorId,
        node_id: NodeId,
    ) -> Result<PlacementDecision, DirectoryError>;

    /// Unregister an actor from the directory.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The actor to unregister
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Unregistration successful
    /// - `Err(DirectoryError)`: Operation failed
    ///
    /// # When to Call
    ///
    /// - Actor deactivates due to idle timeout
    /// - Explicit shutdown request
    /// - Activation failure (after cleanup delay)
    ///
    /// # Idempotency
    ///
    /// Unregistering an already-unregistered actor succeeds (no-op).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Actor idle timeout triggered
    /// actor_context.set_state(ActivationState::Deactivating)?;
    /// actor.on_deactivate(DeactivationReason::IdleTimeout).await?;
    ///
    /// // Unregister from directory
    /// directory.unregister(&actor_id).await?;
    ///
    /// // Remove from local catalog
    /// catalog.remove_activation(&actor_id)?;
    /// ```
    async fn unregister(&self, actor_id: &ActorId) -> Result<(), DirectoryError>;

    /// Get the number of actors currently registered on a specific node.
    ///
    /// # Parameters
    ///
    /// - `node_id`: The node to query
    ///
    /// # Returns
    ///
    /// The count of actors registered on the specified node.
    ///
    /// # Usage
    ///
    /// This method is primarily used by the Placement module to implement
    /// load-aware placement strategies.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = directory.get_actor_count(&node_id).await;
    /// println!("Node {} has {} actors", node_id, count);
    /// ```
    async fn get_actor_count(&self, node_id: &NodeId) -> usize;

    /// Get the current load for all nodes.
    ///
    /// # Returns
    ///
    /// A map of NodeId → actor count for all nodes being tracked.
    ///
    /// # Usage
    ///
    /// This method is used by the Placement module to make load-aware
    /// placement decisions. It returns the complete load state.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use std::collections::HashMap;
    ///
    /// let node_loads = directory.get_all_node_loads().await;
    /// let chosen_node = placement.choose_node(&actor_id, hint, &caller, &node_loads)?;
    /// ```
    async fn get_all_node_loads(&self) -> std::collections::HashMap<NodeId, usize>;

    /// Get all actor IDs registered on a specific node.
    ///
    /// # Parameters
    ///
    /// - `node_id`: The node to query
    ///
    /// # Returns
    ///
    /// A vector of ActorId instances for all actors registered on the specified node.
    ///
    /// # Usage
    ///
    /// This method is primarily used for debugging, monitoring, and displaying
    /// actor distribution across the cluster.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let actors = directory.get_actors_on_node(&node_id).await;
    /// println!("Node {} hosts {} actors:", node_id, actors.len());
    /// for actor_id in actors {
    ///     println!("  - {}", actor_id);
    /// }
    /// ```
    async fn get_actors_on_node(&self, node_id: &NodeId) -> Vec<ActorId>;
}
