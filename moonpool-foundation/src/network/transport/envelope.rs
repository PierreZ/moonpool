use std::fmt::Debug;

use crate::network::transport::types::EnvelopeError;

/// Unified trait for envelope types that carry payloads with correlation IDs
///
/// This trait combines serialization, correlation, and factory concerns into a single interface.
/// It allows applications to use their own envelope types directly with the transport layer.
pub trait Envelope: Debug + Clone + Sized + Send + 'static {
    /// Serialize envelope to bytes for network transmission
    fn to_bytes(&self) -> Vec<u8>;

    /// Deserialize envelope from bytes received from network
    fn from_bytes(data: &[u8]) -> Result<Self, EnvelopeError>;

    /// Try to parse an envelope from a buffer, consuming parsed data
    ///
    /// This is used for streaming reception where messages may arrive in fragments.
    /// The default implementation uses `from_bytes` and assumes the entire buffer
    /// contains one message. Override for zero-copy parsing or partial message handling.
    ///
    /// Returns:
    /// - `Ok(Some(envelope))` - Successfully parsed, data consumed from buffer
    /// - `Ok(None)` - Buffer is empty or insufficient data
    /// - `Err(EnvelopeError::InsufficientData{needed, available})` - Need more bytes
    /// - `Err(other)` - Parse error, malformed data
    fn try_from_buffer(buffer: &mut Vec<u8>) -> Result<Option<Self>, EnvelopeError> {
        if buffer.is_empty() {
            return Ok(None);
        }

        match Self::from_bytes(buffer) {
            Ok(envelope) => {
                buffer.clear(); // Consumed entire buffer
                Ok(Some(envelope))
            }
            Err(EnvelopeError::InsufficientData { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Get correlation ID for request-response matching
    fn correlation_id(&self) -> u64;

    /// Check if this envelope is a response to a given request correlation ID
    fn is_response_to(&self, request_id: u64) -> bool {
        self.correlation_id() == request_id
    }

    /// Create a response envelope from this request with the given payload
    fn create_response(&self, payload: Vec<u8>) -> Self;

    /// Extract the application payload from this envelope
    fn payload(&self) -> &[u8];
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
    fn to_bytes(&self) -> Vec<u8> {
        // Simple format: [correlation_id:8][payload_len:4][payload:N]
        let mut buffer = Vec::with_capacity(12 + self.payload.len());
        buffer.extend_from_slice(&self.correlation_id.to_le_bytes());
        buffer.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&self.payload);
        buffer
    }

    fn from_bytes(data: &[u8]) -> Result<Self, EnvelopeError> {
        if data.len() < 12 {
            return Err(EnvelopeError::InsufficientData {
                needed: 12,
                available: data.len(),
            });
        }

        let correlation_id = u64::from_le_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);

        let payload_len = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;

        let expected_total = 12 + payload_len;
        if data.len() != expected_total {
            return Err(EnvelopeError::DeserializationFailed(format!(
                "Invalid data length: got {} bytes, expected {}",
                data.len(),
                expected_total
            )));
        }

        let payload = data[12..].to_vec();
        Ok(SimpleEnvelope::new(correlation_id, payload))
    }

    fn correlation_id(&self) -> u64 {
        self.correlation_id
    }

    fn create_response(&self, payload: Vec<u8>) -> Self {
        SimpleEnvelope::new(self.correlation_id, payload)
    }

    fn payload(&self) -> &[u8] {
        &self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_envelope_creation() {
        let envelope = SimpleEnvelope::new(42, b"test payload".to_vec());

        assert_eq!(Envelope::correlation_id(&envelope), 42);
        assert_eq!(Envelope::payload(&envelope), b"test payload");
        assert!(!envelope.is_empty());
        assert_eq!(envelope.payload_size(), 12);
    }

    #[test]
    fn test_simple_envelope_empty_payload() {
        let envelope = SimpleEnvelope::new(0, Vec::new());

        assert_eq!(Envelope::correlation_id(&envelope), 0);
        assert_eq!(Envelope::payload(&envelope), b"");
        assert!(envelope.is_empty());
        assert_eq!(envelope.payload_size(), 0);
    }

    #[test]
    fn test_envelope_serialization_round_trip() {
        let original = SimpleEnvelope::new(123, b"request data".to_vec());

        let bytes = original.to_bytes();
        let deserialized = SimpleEnvelope::from_bytes(&bytes).unwrap();

        assert_eq!(Envelope::correlation_id(&deserialized), 123);
        assert_eq!(Envelope::payload(&deserialized), b"request data");
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_envelope_create_response() {
        let request = SimpleEnvelope::new(456, b"original request".to_vec());
        let response = request.create_response(b"reply data".to_vec());

        // Response should have same correlation ID as request
        assert_eq!(
            Envelope::correlation_id(&response),
            Envelope::correlation_id(&request)
        );
        assert_eq!(Envelope::correlation_id(&response), 456);
        assert_eq!(Envelope::payload(&response), b"reply data");
    }

    #[test]
    fn test_response_detection() {
        let envelope = SimpleEnvelope::new(100, b"test".to_vec());

        // Should match its own correlation ID
        assert!(Envelope::is_response_to(&envelope, 100));

        // Should not match different correlation IDs
        assert!(!Envelope::is_response_to(&envelope, 99));
        assert!(!Envelope::is_response_to(&envelope, 101));
    }

    #[test]
    fn test_correlation_id_matching() {
        let correlation_id = 42;
        let request = SimpleEnvelope::new(correlation_id, b"ping".to_vec());
        let response = request.create_response(b"pong".to_vec());

        // Both should have the same correlation ID
        assert_eq!(Envelope::correlation_id(&request), correlation_id);
        assert_eq!(Envelope::correlation_id(&response), correlation_id);

        // Response should be detected as response to the request's correlation ID
        assert!(Envelope::is_response_to(&response, correlation_id));
        assert!(Envelope::is_response_to(&request, correlation_id)); // Request also matches its own ID
    }

    #[test]
    fn test_envelope_clone_and_equality() {
        let envelope1 = SimpleEnvelope::new(1, b"data".to_vec());
        let envelope2 = envelope1.clone();

        assert_eq!(envelope1, envelope2);
        assert_eq!(
            Envelope::correlation_id(&envelope1),
            Envelope::correlation_id(&envelope2)
        );
        assert_eq!(Envelope::payload(&envelope1), Envelope::payload(&envelope2));
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

    #[test]
    fn test_insufficient_data_error() {
        let incomplete_data = vec![1, 2, 3]; // Less than 12 bytes needed

        let result = SimpleEnvelope::from_bytes(&incomplete_data);
        assert!(result.is_err());

        match result.unwrap_err() {
            EnvelopeError::InsufficientData { needed, available } => {
                assert_eq!(needed, 12);
                assert_eq!(available, 3);
            }
            _ => panic!("Expected InsufficientData error"),
        }
    }

    #[test]
    fn test_invalid_length_error() {
        let mut data = Vec::new();
        data.extend_from_slice(&42u64.to_le_bytes()); // correlation_id
        data.extend_from_slice(&10u32.to_le_bytes()); // payload_len says 10
        data.extend_from_slice(b"short"); // but only 5 bytes provided

        let result = SimpleEnvelope::from_bytes(&data);
        assert!(result.is_err());

        match result.unwrap_err() {
            EnvelopeError::DeserializationFailed(msg) => {
                assert!(msg.contains("Invalid data length"));
            }
            _ => panic!("Expected DeserializationFailed error"),
        }
    }
}
