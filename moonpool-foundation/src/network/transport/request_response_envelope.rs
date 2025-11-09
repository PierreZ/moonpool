use std::fmt::Debug;

use crate::network::transport::Envelope;
use crate::network::transport::types::EnvelopeError;

/// Request-response envelope with correlation ID and payload
/// Wire format: \[correlation_id:8\]\[len:4\]\[payload:N\]
#[derive(Debug, Clone, PartialEq)]
pub struct RequestResponseEnvelope {
    /// The correlation ID for matching requests and responses
    pub correlation_id: u64,
    /// The payload data carried by this envelope
    pub payload: Vec<u8>,
}

impl RequestResponseEnvelope {
    /// Create a new RequestResponseEnvelope with the given correlation ID and payload
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

    /// Get the total serialized size in bytes (header + payload)
    pub fn serialized_size(&self) -> usize {
        8 + 4 + self.payload.len() // correlation_id + length + payload
    }
}

impl Envelope for RequestResponseEnvelope {
    fn to_bytes(&self) -> Vec<u8> {
        RequestResponseSerializer::new().serialize(self)
    }

    fn from_bytes(data: &[u8]) -> Result<Self, EnvelopeError> {
        RequestResponseSerializer::new().deserialize(data)
    }

    fn try_from_buffer(buffer: &mut Vec<u8>) -> Result<Option<Self>, EnvelopeError> {
        RequestResponseSerializer::new().try_deserialize_from_buffer(buffer)
    }

    fn correlation_id(&self) -> u64 {
        self.correlation_id
    }

    fn create_response(&self, payload: Vec<u8>) -> Self {
        RequestResponseEnvelope::new(self.correlation_id, payload)
    }

    fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Binary serializer for RequestResponseEnvelope
/// Wire format: \\[correlation_id:8\\]\\[len:4\\]\\[payload:N\\] (little-endian)
#[derive(Clone)]
pub struct RequestResponseSerializer;

impl RequestResponseSerializer {
    /// Create a new RequestResponseSerializer
    pub fn new() -> Self {
        Self
    }

    /// Maximum supported payload size (1MB)
    pub const MAX_PAYLOAD_SIZE: usize = 1024 * 1024;

    /// Header size in bytes (correlation_id + length)
    pub const HEADER_SIZE: usize = 8 + 4;
}

impl Default for RequestResponseSerializer {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestResponseSerializer {
    /// Serialize a RequestResponseEnvelope to bytes
    pub fn serialize(&self, envelope: &RequestResponseEnvelope) -> Vec<u8> {
        let payload_len = envelope.payload.len();
        let total_len = Self::HEADER_SIZE + payload_len;
        let mut buffer = Vec::with_capacity(total_len);

        // Write correlation_id (8 bytes, little-endian)
        buffer.extend_from_slice(&envelope.correlation_id.to_le_bytes());

        // Write payload length (4 bytes, little-endian)
        buffer.extend_from_slice(&(payload_len as u32).to_le_bytes());

        // Write payload
        buffer.extend_from_slice(&envelope.payload);

        buffer
    }

    /// Deserialize bytes to a RequestResponseEnvelope
    pub fn deserialize(&self, data: &[u8]) -> Result<RequestResponseEnvelope, EnvelopeError> {
        // Check minimum size for header
        if data.len() < Self::HEADER_SIZE {
            return Err(EnvelopeError::DeserializationFailed(format!(
                "Data too short: got {} bytes, need at least {}",
                data.len(),
                Self::HEADER_SIZE
            )));
        }

        // Parse correlation_id (8 bytes, little-endian)
        let correlation_id = u64::from_le_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);

        // Parse payload length (4 bytes, little-endian)
        let payload_len = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;

        // Validate payload length
        if payload_len > Self::MAX_PAYLOAD_SIZE {
            return Err(EnvelopeError::DeserializationFailed(format!(
                "Payload too large: {} bytes, max allowed: {}",
                payload_len,
                Self::MAX_PAYLOAD_SIZE
            )));
        }

        let expected_total_len = Self::HEADER_SIZE + payload_len;
        if data.len() != expected_total_len {
            return Err(EnvelopeError::DeserializationFailed(format!(
                "Invalid data length: got {} bytes, expected {} bytes",
                data.len(),
                expected_total_len
            )));
        }

        // Extract payload
        let payload = data[Self::HEADER_SIZE..].to_vec();

        Ok(RequestResponseEnvelope::new(correlation_id, payload))
    }

    /// Try to deserialize from a buffer, consuming parsed data
    pub fn try_deserialize_from_buffer(
        &self,
        buffer: &mut Vec<u8>,
    ) -> Result<Option<RequestResponseEnvelope>, EnvelopeError> {
        let buffer_len_before = buffer.len();

        tracing::warn!("TRY_FROM_BUFFER: buffer_len={}", buffer_len_before);

        // Buffer is empty - normal case, not an error
        if buffer.is_empty() {
            return Ok(None);
        }

        // Need at least header size to proceed
        if buffer.len() < Self::HEADER_SIZE {
            return Err(EnvelopeError::InsufficientData {
                needed: Self::HEADER_SIZE,
                available: buffer.len(),
            });
        }

        // Read correlation_id to know which message we're parsing
        let correlation_id = u64::from_le_bytes([
            buffer[0], buffer[1], buffer[2], buffer[3], buffer[4], buffer[5], buffer[6], buffer[7],
        ]);

        // Read payload length from header
        let payload_len =
            u32::from_le_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]) as usize;

        // Validate payload length
        if payload_len > Self::MAX_PAYLOAD_SIZE {
            return Err(EnvelopeError::DeserializationFailed(format!(
                "Payload too large: {} bytes, max allowed: {}",
                payload_len,
                Self::MAX_PAYLOAD_SIZE
            )));
        }

        let total_message_len = Self::HEADER_SIZE + payload_len;

        // Check if we have the complete message
        if buffer.len() < total_message_len {
            return Err(EnvelopeError::InsufficientData {
                needed: total_message_len,
                available: buffer.len(),
            });
        }

        // Extract the complete message data
        let message_data: Vec<u8> = buffer.drain(0..total_message_len).collect();

        // Deserialize the complete message
        match self.deserialize(&message_data) {
            Ok(envelope) => {
                tracing::warn!(
                    "TRY_FROM_BUFFER_SUCCESS: correlation_id={}, consumed={}, buffer_remaining={}",
                    correlation_id,
                    total_message_len,
                    buffer.len()
                );
                Ok(Some(envelope))
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::transport::Envelope;

    #[test]
    fn test_request_response_envelope_creation() {
        let envelope = RequestResponseEnvelope::new(42, b"test payload".to_vec());

        assert_eq!(envelope.correlation_id(), 42);
        assert_eq!(Envelope::payload(&envelope), b"test payload");
        assert!(!envelope.is_empty());
        assert_eq!(envelope.payload_size(), 12);
        assert_eq!(envelope.serialized_size(), 8 + 4 + 12); // header + payload
    }

    #[test]
    fn test_request_response_envelope_empty_payload() {
        let envelope = RequestResponseEnvelope::new(0, Vec::new());

        assert_eq!(envelope.correlation_id(), 0);
        assert_eq!(Envelope::payload(&envelope), b"");
        assert!(envelope.is_empty());
        assert_eq!(envelope.payload_size(), 0);
        assert_eq!(envelope.serialized_size(), 12); // header only
    }

    #[test]
    fn test_request_response_envelope_factory() {
        let correlation_id = 123;
        let request = RequestResponseEnvelope::new(correlation_id, b"ping".to_vec());
        let reply = request.create_response(b"pong".to_vec());

        // Both should have same correlation ID
        assert_eq!(Envelope::correlation_id(&request), correlation_id);
        assert_eq!(Envelope::correlation_id(&reply), correlation_id);

        // Reply should have matching correlation ID
        assert_eq!(reply.correlation_id, correlation_id);

        // Extract payload should work
        assert_eq!(Envelope::payload(&request), b"ping");
        assert_eq!(Envelope::payload(&reply), b"pong");
    }

    #[test]
    fn test_serialization_roundtrip() {
        let serializer = RequestResponseSerializer::new();
        let original = RequestResponseEnvelope::new(0x123456789ABCDEF0, b"Hello, World!".to_vec());

        // Serialize
        let serialized = serializer.serialize(&original);

        // Deserialize
        let deserialized = serializer
            .deserialize(&serialized)
            .expect("Deserialization should succeed");

        // Should be identical
        assert_eq!(original, deserialized);
        assert_eq!(original.correlation_id, deserialized.correlation_id);
        assert_eq!(original.payload, deserialized.payload);
    }

    #[test]
    fn test_serialization_empty_payload() {
        let serializer = RequestResponseSerializer::new();
        let original = RequestResponseEnvelope::new(42, Vec::new());

        let serialized = serializer.serialize(&original);
        let deserialized = serializer
            .deserialize(&serialized)
            .expect("Deserialization should succeed");

        assert_eq!(original, deserialized);
        assert_eq!(deserialized.payload_size(), 0);
    }

    #[test]
    fn test_serialization_wire_format() {
        let serializer = RequestResponseSerializer::new();
        let envelope = RequestResponseEnvelope::new(0x0102030405060708, b"AB".to_vec());

        let serialized = serializer.serialize(&envelope);

        // Check wire format: [correlation_id:8][len:4][payload:N]
        assert_eq!(serialized.len(), 8 + 4 + 2);

        // correlation_id (little-endian)
        assert_eq!(
            &serialized[0..8],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );

        // payload length (little-endian)
        assert_eq!(&serialized[8..12], &[0x02, 0x00, 0x00, 0x00]); // 2 as u32 LE

        // payload
        assert_eq!(&serialized[12..14], b"AB");
    }

    #[test]
    fn test_deserialization_too_short() {
        let serializer = RequestResponseSerializer::new();

        // Too short for header
        let result = serializer.deserialize(&[1, 2, 3]);
        assert!(matches!(
            result,
            Err(EnvelopeError::DeserializationFailed(_))
        ));

        // Header only, but claims to have payload
        let mut data = vec![0u8; 12];
        data[8..12].copy_from_slice(&5u32.to_le_bytes()); // claims 5 byte payload
        let result = serializer.deserialize(&data);
        assert!(matches!(
            result,
            Err(EnvelopeError::DeserializationFailed(_))
        ));
    }

    #[test]
    fn test_deserialization_payload_too_large() {
        let serializer = RequestResponseSerializer::new();

        // Create data claiming massive payload
        let mut data = vec![0u8; 12];
        let massive_size = RequestResponseSerializer::MAX_PAYLOAD_SIZE + 1;
        data[8..12].copy_from_slice(&(massive_size as u32).to_le_bytes());

        let result = serializer.deserialize(&data);
        assert!(matches!(
            result,
            Err(EnvelopeError::DeserializationFailed(_))
        ));
    }

    #[test]
    fn test_deserialization_length_mismatch() {
        let serializer = RequestResponseSerializer::new();

        // Create valid header but wrong total length
        let mut data = vec![0u8; 15]; // 12 header + 3 bytes
        data[8..12].copy_from_slice(&5u32.to_le_bytes()); // claims 5 byte payload, but only 3 provided

        let result = serializer.deserialize(&data);
        assert!(matches!(
            result,
            Err(EnvelopeError::DeserializationFailed(_))
        ));
    }

    #[test]
    fn test_clone_and_equality() {
        let envelope1 = RequestResponseEnvelope::new(999, b"test data".to_vec());
        let envelope2 = envelope1.clone();

        assert_eq!(envelope1, envelope2);
        assert_eq!(envelope1.correlation_id, envelope2.correlation_id);
        assert_eq!(envelope1.payload, envelope2.payload);
    }

    #[test]
    fn test_serializer_default() {
        let serializer1 = RequestResponseSerializer::new();
        let serializer2 = RequestResponseSerializer;

        // Both should work the same way
        let envelope = RequestResponseEnvelope::new(1, b"test".to_vec());
        let ser1 = serializer1.serialize(&envelope);
        let ser2 = serializer2.serialize(&envelope);

        assert_eq!(ser1, ser2);
    }

    #[test]
    fn test_try_deserialize_from_buffer_insufficient_data() {
        let serializer = RequestResponseSerializer::new();

        // Test empty buffer
        let mut empty_buffer = Vec::new();
        let result = serializer.try_deserialize_from_buffer(&mut empty_buffer);
        assert_eq!(result, Ok(None));

        // Test partial header (only 6 bytes, need 12)
        let mut partial_header = vec![1, 2, 3, 4, 5, 6];
        let result = serializer.try_deserialize_from_buffer(&mut partial_header);
        assert_eq!(
            result,
            Err(
                crate::network::transport::types::EnvelopeError::InsufficientData {
                    needed: 12,
                    available: 6,
                }
            )
        );

        // Test complete header but incomplete message
        let envelope = RequestResponseEnvelope::new(42, b"hello world".to_vec());
        let complete_data = serializer.serialize(&envelope);
        let mut partial_message = complete_data[0..15].to_vec(); // Header (12) + 3 bytes of payload

        let result = serializer.try_deserialize_from_buffer(&mut partial_message);
        assert_eq!(
            result,
            Err(
                crate::network::transport::types::EnvelopeError::InsufficientData {
                    needed: 23, // 12 header + 11 payload
                    available: 15,
                }
            )
        );

        // Verify buffer was not consumed on error
        assert_eq!(partial_message.len(), 15);
    }

    #[test]
    fn test_try_deserialize_from_buffer_complete_message() {
        let serializer = RequestResponseSerializer::new();
        let envelope = RequestResponseEnvelope::new(123, b"test data".to_vec());
        let serialized = serializer.serialize(&envelope);

        let mut buffer = serialized.clone();
        let result = serializer.try_deserialize_from_buffer(&mut buffer);

        match result {
            Ok(Some(deserialized)) => {
                assert_eq!(deserialized.correlation_id, 123);
                assert_eq!(deserialized.payload, b"test data");
                assert!(buffer.is_empty()); // Buffer should be consumed
            }
            _ => panic!("Expected successful deserialization"),
        }
    }
}
