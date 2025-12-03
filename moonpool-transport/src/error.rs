//! Error types for the moonpool messaging layer.

use crate::UID;

/// Errors that can occur in the messaging layer.
#[derive(Debug, thiserror::Error)]
pub enum MessagingError {
    /// Endpoint not found for the given token.
    #[error("endpoint not found: {token}")]
    EndpointNotFound {
        /// The token that was not found.
        token: UID,
    },

    /// Failed to deserialize a message.
    #[error("deserialization failed: {message}")]
    DeserializationFailed {
        /// Details about the deserialization failure.
        message: String,
    },

    /// Failed to serialize a message.
    #[error("serialization failed: {message}")]
    SerializationFailed {
        /// Details about the serialization failure.
        message: String,
    },

    /// Peer connection error.
    #[error("peer error: {message}")]
    PeerError {
        /// Details about the peer error.
        message: String,
    },

    /// Queue is full and cannot accept more messages.
    #[error("queue full: capacity {capacity}")]
    QueueFull {
        /// Maximum capacity of the queue.
        capacity: usize,
    },

    /// Invalid well-known token index.
    #[error("invalid well-known token: {index} (max: {max})")]
    InvalidWellKnownToken {
        /// The invalid token index.
        index: u32,
        /// Maximum allowed index.
        max: usize,
    },

    /// Transport is closed or shutting down.
    #[error("transport closed")]
    TransportClosed,

    /// Invalid state for the requested operation.
    #[error("invalid state: {message}")]
    InvalidState {
        /// Details about the invalid state.
        message: String,
    },

    /// Network operation failed.
    #[error("network error: {message}")]
    NetworkError {
        /// Details about the network error.
        message: String,
    },
}

impl From<serde_json::Error> for MessagingError {
    fn from(err: serde_json::Error) -> Self {
        MessagingError::DeserializationFailed {
            message: err.to_string(),
        }
    }
}

impl From<crate::PeerError> for MessagingError {
    fn from(err: crate::PeerError) -> Self {
        MessagingError::PeerError {
            message: err.to_string(),
        }
    }
}
