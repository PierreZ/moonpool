use std::fmt::Debug;

/// Core trait for all envelope types that carry payloads
pub trait Envelope: Debug + Clone {
    /// Get the payload data
    fn payload(&self) -> &[u8];
}

/// Factory trait for creating envelopes with correlation IDs
pub trait EnvelopeFactory<E: Envelope> {
    /// Create a new request envelope with the given correlation ID and payload
    fn create_request(correlation_id: u64, payload: Vec<u8>) -> E;

    /// Create a reply envelope from the given request envelope
    fn create_reply(request: &E, payload: Vec<u8>) -> E;

    /// Extract the payload from an envelope
    fn extract_payload(envelope: &E) -> &[u8];
}

/// Trait for detecting if an envelope is a reply to a specific correlation ID
pub trait EnvelopeReplyDetection {
    /// Check if this envelope is a reply to the given correlation ID
    fn is_reply_to(&self, correlation_id: u64) -> bool;

    /// Get the correlation ID of this envelope
    fn correlation_id(&self) -> Option<u64>;
}

/// Simple envelope implementation for testing and basic use cases
#[derive(Debug, Clone, PartialEq)]
pub struct SimpleEnvelope {
    /// The correlation ID for matching requests and responses
    pub correlation_id: u64,
    /// The payload data carried by this envelope
    pub payload: Vec<u8>,
}

impl SimpleEnvelope {
    /// Create a new SimpleEnvelope with the given correlation ID and payload
    pub fn new(correlation_id: u64, payload: Vec<u8>) -> Self {
        Self {
            correlation_id,
            payload,
        }
    }

    /// Check if the payload is empty
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    /// Get the payload size in bytes
    pub fn payload_size(&self) -> usize {
        self.payload.len()
    }
}

impl Envelope for SimpleEnvelope {
    fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl EnvelopeReplyDetection for SimpleEnvelope {
    fn is_reply_to(&self, correlation_id: u64) -> bool {
        self.correlation_id == correlation_id
    }

    fn correlation_id(&self) -> Option<u64> {
        Some(self.correlation_id)
    }
}

/// Factory for creating SimpleEnvelope instances
pub struct SimpleEnvelopeFactory;

impl EnvelopeFactory<SimpleEnvelope> for SimpleEnvelopeFactory {
    fn create_request(correlation_id: u64, payload: Vec<u8>) -> SimpleEnvelope {
        SimpleEnvelope::new(correlation_id, payload)
    }

    fn create_reply(request: &SimpleEnvelope, payload: Vec<u8>) -> SimpleEnvelope {
        SimpleEnvelope::new(request.correlation_id, payload)
    }

    fn extract_payload(envelope: &SimpleEnvelope) -> &[u8] {
        envelope.payload()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_envelope_creation() {
        let envelope = SimpleEnvelope::new(42, b"test payload".to_vec());

        assert_eq!(EnvelopeReplyDetection::correlation_id(&envelope), Some(42));
        assert_eq!(envelope.payload(), b"test payload");
        assert!(!envelope.is_empty());
        assert_eq!(envelope.payload_size(), 12);
    }

    #[test]
    fn test_simple_envelope_empty_payload() {
        let envelope = SimpleEnvelope::new(0, Vec::new());

        assert_eq!(EnvelopeReplyDetection::correlation_id(&envelope), Some(0));
        assert_eq!(envelope.payload(), b"");
        assert!(envelope.is_empty());
        assert_eq!(envelope.payload_size(), 0);
    }

    #[test]
    fn test_envelope_factory_create_request() {
        let envelope = SimpleEnvelopeFactory::create_request(123, b"request data".to_vec());

        assert_eq!(EnvelopeReplyDetection::correlation_id(&envelope), Some(123));
        assert_eq!(envelope.payload(), b"request data");
    }

    #[test]
    fn test_envelope_factory_create_reply() {
        let request = SimpleEnvelope::new(456, b"original request".to_vec());
        let reply = SimpleEnvelopeFactory::create_reply(&request, b"reply data".to_vec());

        // Reply should have same correlation ID as request
        assert_eq!(
            EnvelopeReplyDetection::correlation_id(&reply),
            EnvelopeReplyDetection::correlation_id(&request)
        );
        assert_eq!(EnvelopeReplyDetection::correlation_id(&reply), Some(456));
        assert_eq!(reply.payload(), b"reply data");
    }

    #[test]
    fn test_envelope_factory_extract_payload() {
        let envelope = SimpleEnvelope::new(789, b"extracted payload".to_vec());
        let payload = SimpleEnvelopeFactory::extract_payload(&envelope);

        assert_eq!(payload, b"extracted payload");
    }

    #[test]
    fn test_reply_detection() {
        let envelope = SimpleEnvelope::new(100, b"test".to_vec());

        // Should match its own correlation ID
        assert!(envelope.is_reply_to(100));

        // Should not match different correlation IDs
        assert!(!envelope.is_reply_to(99));
        assert!(!envelope.is_reply_to(101));

        // Get correlation ID should return the envelope's ID
        assert_eq!(EnvelopeReplyDetection::correlation_id(&envelope), Some(100));
    }

    #[test]
    fn test_correlation_id_matching() {
        let correlation_id = 42;
        let request = SimpleEnvelopeFactory::create_request(correlation_id, b"ping".to_vec());
        let reply = SimpleEnvelopeFactory::create_reply(&request, b"pong".to_vec());

        // Both should have the same correlation ID
        assert_eq!(
            EnvelopeReplyDetection::correlation_id(&request),
            Some(correlation_id)
        );
        assert_eq!(
            EnvelopeReplyDetection::correlation_id(&reply),
            Some(correlation_id)
        );

        // Reply should be detected as reply to the request's correlation ID
        assert!(reply.is_reply_to(correlation_id));
        assert!(request.is_reply_to(correlation_id)); // Request also matches its own ID
    }

    #[test]
    fn test_envelope_clone_and_equality() {
        let envelope1 = SimpleEnvelope::new(1, b"data".to_vec());
        let envelope2 = envelope1.clone();

        assert_eq!(envelope1, envelope2);
        assert_eq!(
            EnvelopeReplyDetection::correlation_id(&envelope1),
            EnvelopeReplyDetection::correlation_id(&envelope2)
        );
        assert_eq!(envelope1.payload(), envelope2.payload());
    }

    #[test]
    fn test_envelope_debug_display() {
        let envelope = SimpleEnvelope::new(999, b"debug test".to_vec());
        let debug_str = format!("{:?}", envelope);

        // Should contain correlation ID and payload in debug output
        assert!(debug_str.contains("999"));
        // The payload is a Vec<u8>, so "debug test" will be shown as byte array
        assert!(
            debug_str.contains("100, 101, 98, 117, 103, 32, 116, 101, 115, 116")
                || debug_str.contains("[100, 101, 98, 117, 103, 32, 116, 101, 115, 116]")
        );
    }
}
