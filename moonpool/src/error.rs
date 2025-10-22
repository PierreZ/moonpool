//! Error types for the moonpool actor system.

use crate::actor::{ActivationState, NodeId};
use thiserror::Error;

/// Errors related to actor operations and lifecycle.
#[derive(Debug, Error)]
pub enum ActorError {
    /// Actor activation failed.
    #[error("Actor activation failed: {0}")]
    ActivationFailed(String),

    /// Actor is in an invalid state for the requested operation.
    #[error("Invalid actor state transition from {from:?} to {to:?}")]
    InvalidStateTransition {
        from: ActivationState,
        to: ActivationState,
    },

    /// Actor method execution failed.
    #[error("Actor method execution failed: {0}")]
    ExecutionFailed(String),

    /// Requested actor method not found.
    #[error("Unknown actor method: {0}")]
    UnknownMethod(String),

    /// Request timed out waiting for response.
    #[error("Request timed out")]
    Timeout,

    /// Target node is unavailable.
    #[error("Node unavailable: {0:?}")]
    NodeUnavailable(NodeId),

    /// Message processing failed.
    #[error("Message processing failed: {0}")]
    ProcessingFailed(String),

    /// Insufficient funds (example domain error).
    #[error("Insufficient funds: have {available}, need {requested}")]
    InsufficientFunds { available: u64, requested: u64 },

    /// Storage operation failed.
    #[error("Storage failed: {0}")]
    StorageFailed(#[from] StorageError),

    /// Message error.
    #[error("Message error: {0}")]
    Message(#[from] MessageError),

    /// Directory error.
    #[error("Directory error: {0}")]
    Directory(#[from] DirectoryError),

    /// Correlation ID not found (late response).
    #[error("Unknown correlation ID")]
    UnknownCorrelationId,

    /// Too many message forwards (routing loop protection).
    #[error("Too many forwards (max: {max})")]
    TooManyForwards { max: u8 },
}

/// Errors related to message handling.
#[derive(Debug, Error)]
pub enum MessageError {
    /// Message serialization failed.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Message deserialization failed.
    #[error("Deserialization failed: {0}")]
    DeserializationFailed(String),

    /// Message exceeds maximum size.
    #[error("Message too large: {size} bytes (max: {max} bytes)")]
    MessageTooLarge { size: usize, max: usize },

    /// Invalid message format.
    #[error("Invalid message format: {0}")]
    InvalidFormat(String),

    /// Message envelope deserialization failed.
    #[error("Envelope deserialization failed: {0}")]
    EnvelopeError(String),

    /// Network I/O error.
    #[error("Network I/O error: {0}")]
    IoError(std::io::Error),
}

// Manual From implementation for io::Error to avoid conflict with serde_json::Error
impl From<std::io::Error> for MessageError {
    fn from(err: std::io::Error) -> Self {
        MessageError::IoError(err)
    }
}

/// Errors related to directory operations.
#[derive(Debug, Error)]
pub enum DirectoryError {
    /// Actor already registered on a different node.
    #[error("Actor already registered on node: {0:?}")]
    AlreadyRegistered(NodeId),

    /// Directory operation timed out.
    #[error("Directory operation timed out")]
    Timeout,

    /// Directory is unavailable.
    #[error("Directory unavailable")]
    Unavailable,

    /// Concurrent activation race detected.
    #[error("Activation race detected: winner {winner:?}, loser {loser:?}")]
    ActivationRace { winner: NodeId, loser: NodeId },

    /// Generic directory operation failed.
    #[error("Directory operation failed: {0}")]
    OperationFailed(String),
}

/// Errors related to storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Serialization/deserialization failed.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Underlying I/O error (file, network, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Storage system unavailable.
    #[error("Storage unavailable")]
    Unavailable,

    /// Key not found (only for operations that require existence).
    #[error("Key not found: {0}")]
    NotFound(String),

    /// Generic storage operation failure.
    #[error("Storage operation failed: {0}")]
    OperationFailed(String),
}

/// Errors related to ActorId parsing.
#[derive(Debug, Error)]
pub enum ActorIdError {
    /// Invalid ActorId format.
    #[error("Invalid ActorId format (expected 'namespace::actor_type/key')")]
    InvalidFormat,

    /// Empty field in ActorId.
    #[error("ActorId field cannot be empty: {0}")]
    EmptyField(String),
}

/// Errors related to NodeId parsing.
#[derive(Debug, Error)]
pub enum NodeIdError {
    /// Invalid NodeId format.
    #[error("Invalid NodeId format (expected 'host:port')")]
    InvalidFormat,

    /// Invalid network address.
    #[error("Invalid network address")]
    InvalidAddress,
}
