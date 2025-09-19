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
