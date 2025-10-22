//! Placement decision algorithms for actor location.
//!
//! This module defines how the directory decides where to place new actor activations
//! and how it resolves concurrent activation attempts.

use crate::actor::NodeId;

/// Result of a directory registration attempt.
///
/// When a node attempts to register an actor, the directory returns a placement
/// decision indicating whether the activation should proceed, forward to another
/// node, or resolve a race condition.
///
/// # Variants
///
/// - **`PlaceOnNode`**: Registration successful, create the actor here
/// - **`AlreadyRegistered`**: Actor exists elsewhere, forward messages there
/// - **`Race`**: Concurrent activation detected, coordinate resolution
///
/// # Race Handling
///
/// When multiple nodes simultaneously try to activate the same actor:
///
/// 1. Directory detects the race (same ActorId, different NodeId)
/// 2. Returns `Race { winner, loser }` to both nodes
/// 3. Winner continues activation
/// 4. Loser deactivates and removes partial activation
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::directory::PlacementDecision;
///
/// match directory.register(actor_id.clone(), my_node_id.clone()).await? {
///     PlacementDecision::PlaceOnNode(node_id) => {
///         // We won (or no race occurred)
///         assert_eq!(node_id, my_node_id);
///         create_activation(actor_id).await?;
///     }
///     PlacementDecision::AlreadyRegistered(node_id) => {
///         // Actor already active on another node
///         forward_message(node_id, message).await?;
///     }
///     PlacementDecision::Race { winner, loser } => {
///         // Concurrent activation detected
///         if loser == my_node_id {
///             // We lost, clean up
///             remove_partial_activation(actor_id).await?;
///         } else {
///             // We won, continue
///             finalize_activation(actor_id).await?;
///         }
///     }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementDecision {
    /// Place actor on the specified node.
    ///
    /// Returned when:
    /// - Actor not currently registered anywhere
    /// - Registration is the first to arrive
    /// - Caller won a concurrent activation race
    ///
    /// The node should proceed with actor activation.
    PlaceOnNode(NodeId),

    /// Actor is already registered on another node.
    ///
    /// Returned when:
    /// - Actor was previously activated elsewhere
    /// - Registration attempt arrives after another node already registered
    ///
    /// The node should forward messages to the existing activation instead
    /// of creating a new one.
    AlreadyRegistered(NodeId),

    /// Concurrent activation race detected.
    ///
    /// Returned when:
    /// - Multiple nodes simultaneously attempt to register same actor
    /// - Directory detected conflicting registration requests
    ///
    /// # Fields
    ///
    /// - `winner`: Node that won the race, activation proceeds here
    /// - `loser`: Node that lost the race, should deactivate
    ///
    /// Both nodes receive this decision. Each checks if they're the loser
    /// and cleans up accordingly.
    Race {
        /// Node that won the activation race
        winner: NodeId,
        /// Node that lost the activation race
        loser: NodeId,
    },
}

impl PlacementDecision {
    /// Check if this decision indicates successful placement.
    ///
    /// Returns `true` for `PlaceOnNode`, `false` otherwise.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let decision = directory.register(actor_id, node_id).await?;
    /// if decision.is_successful() {
    ///     create_activation().await?;
    /// }
    /// ```
    pub fn is_successful(&self) -> bool {
        matches!(self, PlacementDecision::PlaceOnNode(_))
    }

    /// Check if this is a race condition.
    ///
    /// Returns `true` for `Race`, `false` otherwise.
    pub fn is_race(&self) -> bool {
        matches!(self, PlacementDecision::Race { .. })
    }

    /// Check if actor is already registered elsewhere.
    ///
    /// Returns `true` for `AlreadyRegistered`, `false` otherwise.
    pub fn is_already_registered(&self) -> bool {
        matches!(self, PlacementDecision::AlreadyRegistered(_))
    }

    /// Get the target node for messaging.
    ///
    /// Returns the node where messages should be sent:
    /// - `PlaceOnNode(node)` → `Some(node)` (local activation)
    /// - `AlreadyRegistered(node)` → `Some(node)` (existing activation)
    /// - `Race { winner, .. }` → `Some(winner)` (race winner)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let decision = directory.register(actor_id, node_id).await?;
    /// if let Some(target) = decision.target_node() {
    ///     send_message_to(target, message).await?;
    /// }
    /// ```
    pub fn target_node(&self) -> Option<&NodeId> {
        match self {
            PlacementDecision::PlaceOnNode(node) => Some(node),
            PlacementDecision::AlreadyRegistered(node) => Some(node),
            PlacementDecision::Race { winner, .. } => Some(winner),
        }
    }
}
