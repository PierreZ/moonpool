//! Error types for peer operations.

use std::io;
use thiserror::Error;

/// Errors that can occur during peer operations.
#[derive(Error, Debug, Clone)]
pub enum PeerError {
    /// Connection failed and could not be established
    #[error("Connection to peer failed")]
    ConnectionFailed,

    /// Connection was lost during operation
    #[error("Connection lost during operation")]
    ConnectionLost,

    /// Message queue is full
    #[error("Message queue is full, cannot queue more messages")]
    QueueFull,

    /// Peer is disconnected and cannot perform operation
    #[error("Peer is disconnected")]
    Disconnected,

    /// Connection timeout occurred
    #[error("Connection timeout")]
    Timeout,

    /// I/O operation failed
    #[error("I/O error: {0}")]
    Io(String),

    /// Invalid operation for current peer state
    #[error("Invalid operation: {0}")]
    InvalidOperation(String),

    /// Receiver has been taken via take_receiver()
    #[error("Receiver has been taken")]
    ReceiverTaken,
}

impl From<io::Error> for PeerError {
    fn from(error: io::Error) -> Self {
        PeerError::Io(error.to_string())
    }
}

/// Result type for peer operations.
pub type PeerResult<T> = Result<T, PeerError>;
