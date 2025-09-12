use std::fmt::Debug;

/// Trait for swappable envelope serialization strategies.
///
/// Users can implement this trait to provide custom envelope formats.
/// This allows for future enhancements like Orleans-style rich metadata
/// or custom user implementations without modifying the core protocol logic.
pub trait EnvelopeSerializer: Clone {
    /// The envelope type that this serializer handles
    type Envelope: Debug + Clone;

    /// Serialize envelope to bytes for network transmission
    fn serialize(&self, envelope: &Self::Envelope) -> Vec<u8>;

    /// Deserialize bytes back to envelope
    fn deserialize(&self, data: &[u8]) -> Result<Self::Envelope, SerializationError>;
}

/// Trait for envelope factory that creates envelopes from payloads.
///
/// This trait enables automatic envelope creation with correlation IDs,
/// providing a clean API that hides envelope construction from application code.
pub trait EnvelopeFactory<S: EnvelopeSerializer> {
    /// Create a request envelope with auto-generated correlation ID
    fn create_request(correlation_id: u64, payload: Vec<u8>) -> S::Envelope;
    
    /// Create a reply envelope from an incoming request
    fn create_reply(request: &S::Envelope, payload: Vec<u8>) -> S::Envelope;
    
    /// Extract payload from an envelope
    fn extract_payload(envelope: &S::Envelope) -> &[u8];
}

/// Trait for envelope reply detection.
///
/// This trait allows FlowTransport to determine if an envelope is a reply
/// to a previous request, enabling proper request-response correlation.
pub trait EnvelopeReplyDetection {
    /// Check if this envelope is a reply to a request with the given correlation ID
    fn is_reply_to(&self, correlation_id: u64) -> bool;

    /// Get the correlation ID of this envelope, if any
    fn correlation_id(&self) -> Option<u64>;
}

/// Errors that can occur during envelope serialization/deserialization
#[derive(Debug, Clone, PartialEq)]
pub enum SerializationError {
    /// Data is too short to contain a valid envelope
    TooShort,
    /// Data format is invalid or corrupted
    InvalidFormat,
    /// UTF-8 conversion failed
    InvalidUtf8,
}
