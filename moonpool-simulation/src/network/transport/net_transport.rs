use async_trait::async_trait;

use super::EnvelopeSerializer;

/// High-level transport abstraction similar to FoundationDB's FlowTransport.
///
/// This trait provides a clean API for network transport that can be implemented
/// by different transport strategies (server, client, etc.) while maintaining
/// a consistent interface for application code.
#[async_trait(?Send)]
pub trait NetTransport<S: EnvelopeSerializer> {
    /// Bind to address and start accepting connections (server mode)
    async fn bind(&mut self, address: &str) -> Result<(), TransportError>;

    /// Send envelope to destination
    fn send(&mut self, destination: &str, envelope: S::Envelope) -> Result<(), TransportError>;

    /// Poll for received envelopes (non-blocking)
    fn poll_receive(&mut self) -> Option<ReceivedEnvelope<S::Envelope>>;

    /// Drive transport (call regularly to process I/O)
    async fn tick(&mut self);

    /// Close transport and clean up resources
    async fn close(&mut self);
}

/// A received envelope with source information
#[derive(Debug, Clone)]
pub struct ReceivedEnvelope<E> {
    /// Address/identifier of the sender
    pub from: String,
    /// The received envelope
    pub envelope: E,
}

/// Errors that can occur in transport operations
#[derive(Debug, Clone, PartialEq)]
pub enum TransportError {
    /// Failed to bind to the specified address
    BindFailed(String),
    /// Failed to send message to destination
    SendFailed(String),
    /// Transport is not bound (server mode only)
    NotBound,
    /// Connection error
    ConnectionError(String),
    /// Serialization error
    SerializationError(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::BindFailed(msg) => write!(f, "Bind failed: {}", msg),
            TransportError::SendFailed(msg) => write!(f, "Send failed: {}", msg),
            TransportError::NotBound => write!(f, "Transport not bound"),
            TransportError::ConnectionError(msg) => write!(f, "Connection error: {}", msg),
            TransportError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
        }
    }
}

impl std::error::Error for TransportError {}
