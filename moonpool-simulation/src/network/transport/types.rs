use std::fmt;

/// Errors that can occur during envelope operations
#[derive(Debug, Clone, PartialEq)]
pub enum EnvelopeError {
    /// Invalid correlation ID provided
    InvalidCorrelationId,
    /// Empty payload when payload is required
    EmptyPayload,
    /// Invalid envelope type or format
    InvalidEnvelopeType,
    /// Serialization failed
    SerializationFailed(String),
    /// Deserialization failed
    DeserializationFailed(String),
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvelopeError::InvalidCorrelationId => write!(f, "Invalid correlation ID"),
            EnvelopeError::EmptyPayload => write!(f, "Empty payload"),
            EnvelopeError::InvalidEnvelopeType => write!(f, "Invalid envelope type"),
            EnvelopeError::SerializationFailed(msg) => write!(f, "Serialization failed: {}", msg),
            EnvelopeError::DeserializationFailed(msg) => {
                write!(f, "Deserialization failed: {}", msg)
            }
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Errors that can occur during transport operations
#[derive(Debug, Clone)]
pub enum TransportError {
    /// Peer operation failed
    PeerError(String),
    /// Envelope serialization/deserialization failed
    SerializationError(String),
    /// Transport disconnected
    Disconnected,
    /// Operation timed out
    Timeout,
    /// Failed to bind to address
    BindFailed(String),
    /// Send operation failed
    SendFailed(String),
    /// Network I/O error
    IoError(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::PeerError(msg) => write!(f, "Peer error: {}", msg),
            TransportError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
            TransportError::Disconnected => write!(f, "Transport disconnected"),
            TransportError::Timeout => write!(f, "Operation timed out"),
            TransportError::BindFailed(msg) => write!(f, "Bind failed: {}", msg),
            TransportError::SendFailed(msg) => write!(f, "Send failed: {}", msg),
            TransportError::IoError(msg) => write!(f, "I/O error: {}", msg),
        }
    }
}

impl std::error::Error for TransportError {}

/// Received envelope with source information
#[derive(Debug, Clone)]
pub struct ReceivedEnvelope<E> {
    /// The received envelope
    pub envelope: E,
    /// Source address or identifier
    pub from: String,
}

impl<E> ReceivedEnvelope<E> {
    /// Create a new ReceivedEnvelope
    pub fn new(envelope: E, from: String) -> Self {
        Self { envelope, from }
    }
}
