//! Directory trait definitions.
//!
//! The Directory is responsible for tracking actor locations across the cluster.

use crate::actor::{ActorId, NodeId, PlacementHint};
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
/// 2. **Placement Decisions**: Choose node for new actor activations
/// 3. **Race Detection**: Handle concurrent activation attempts
/// 4. **Cache Management**: Coordinate cache invalidation across nodes
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

    /// Choose a node for placing a new actor activation.
    ///
    /// # Parameters
    ///
    /// - `hint`: Placement hint from the actor (Local, Random, or LeastLoaded)
    /// - `caller_node`: The node making the placement request
    ///
    /// # Returns
    ///
    /// - `Ok(node_id)`: Recommended node for activation
    /// - `Err(DirectoryError)`: No suitable node available
    ///
    /// # Placement Strategy
    ///
    /// - **PlacementHint::Local**: Return `caller_node` (prefer local activation)
    /// - **PlacementHint::Random**: Choose a random node from the cluster (uniform distribution)
    /// - **PlacementHint::LeastLoaded**: Choose the node with fewest active actors (load balancing)
    ///
    /// # Load Balancing
    ///
    /// The `LeastLoaded` hint uses the directory's internal load tracking (`node_load` map)
    /// to select the node with the minimum activation count. This provides automatic
    /// load balancing based on current cluster state.
    ///
    /// # Usage
    ///
    /// Called when a message arrives for an actor that doesn't exist anywhere:
    ///
    /// ```rust,ignore
    /// if directory.lookup(&actor_id).await?.is_none() {
    ///     let hint = MyActor::placement_hint();
    ///     let target_node = directory.choose_placement_node(hint, my_node_id).await?;
    ///     directory.register(actor_id, target_node).await?;
    /// }
    /// ```
    async fn choose_placement_node(
        &self,
        hint: PlacementHint,
        caller_node: &NodeId,
    ) -> Result<NodeId, DirectoryError>;

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
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = directory.get_actor_count(&node_id).await;
    /// println!("Node {} has {} actors", node_id, count);
    /// ```
    async fn get_actor_count(&self, node_id: &NodeId) -> usize;
}
