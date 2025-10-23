//! Remote activation messages for Orleans-style actor placement.
//!
//! This module provides message types for triggering actor activation on remote nodes.
//! When a message arrives for an actor that doesn't exist anywhere in the cluster,
//! the directory chooses a target node and sends an ActivationRequest to that node.

use crate::actor::{ActorId, NodeId};
use serde::{Deserialize, Serialize};

/// Request to activate an actor on a remote node.
///
/// This message is sent when:
/// - A message arrives for an actor
/// - directory.lookup() returns None (actor not activated anywhere)
/// - directory.choose_placement_node() selects a target node
/// - The target node is remote (not local)
///
/// # Flow
///
/// ```text
/// Node A receives message for actor X:
///   1. directory.lookup(X) → None
///   2. directory.choose_placement_node() → Node B
///   3. Send ActivationRequest(X) to Node B
///   4. Node B activates actor X
///   5. Node B responds with ActivationResponse
///   6. Node A forwards original message to Node B
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationRequest {
    /// The actor to activate.
    pub actor_id: ActorId,

    /// The node requesting activation (for response routing).
    pub requesting_node: NodeId,
}

/// Response to an activation request.
///
/// Contains the placement decision from the target node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationResponse {
    /// The actor that was requested.
    pub actor_id: ActorId,

    /// The result of the activation attempt.
    pub result: ActivationResult,
}

/// Result of an activation attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActivationResult {
    /// Activation successful on this node.
    Success {
        /// The node where the actor was activated.
        node_id: NodeId,
    },

    /// Actor was already activated on a different node.
    AlreadyExists {
        /// The node where the actor already exists.
        node_id: NodeId,
    },

    /// Activation failed due to an error.
    Failed {
        /// Error message describing the failure.
        error: String,
    },
}

impl ActivationRequest {
    /// Create a new activation request.
    pub fn new(actor_id: ActorId, requesting_node: NodeId) -> Self {
        Self {
            actor_id,
            requesting_node,
        }
    }
}

impl ActivationResponse {
    /// Create a successful activation response.
    pub fn success(actor_id: ActorId, node_id: NodeId) -> Self {
        Self {
            actor_id,
            result: ActivationResult::Success { node_id },
        }
    }

    /// Create an already-exists response.
    pub fn already_exists(actor_id: ActorId, node_id: NodeId) -> Self {
        Self {
            actor_id,
            result: ActivationResult::AlreadyExists { node_id },
        }
    }

    /// Create a failed activation response.
    pub fn failed(actor_id: ActorId, error: String) -> Self {
        Self {
            actor_id,
            result: ActivationResult::Failed { error },
        }
    }

    /// Get the node ID from the result if available.
    pub fn node_id(&self) -> Option<&NodeId> {
        match &self.result {
            ActivationResult::Success { node_id } | ActivationResult::AlreadyExists { node_id } => {
                Some(node_id)
            }
            ActivationResult::Failed { .. } => None,
        }
    }

    /// Check if the activation was successful.
    pub fn is_success(&self) -> bool {
        matches!(self.result, ActivationResult::Success { .. })
    }
}
