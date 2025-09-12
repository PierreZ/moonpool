use super::{EnvelopeFactory, EnvelopeReplyDetection, EnvelopeSerializer, SerializationError};

/// Request-response envelope for correlation (FDB-inspired).
///
/// This envelope format enables request-response correlation using a correlation ID
/// that links requests with their responses.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestResponseEnvelope {
    /// Correlation ID to link requests and responses
    pub correlation_id: u64,
    /// Raw application data payload
    pub payload: Vec<u8>,
}

/// Serializer for RequestResponseEnvelope using a fixed binary format.
///
/// Wire format: [correlation_id:8][len:4][payload:N]
/// - correlation_id: correlation identifier (8 bytes, little-endian u64)
/// - len: payload length (4 bytes, little-endian u32)
/// - payload: raw payload bytes (N bytes)
#[derive(Clone)]
pub struct RequestResponseSerializer;

impl EnvelopeSerializer for RequestResponseSerializer {
    type Envelope = RequestResponseEnvelope;

    fn serialize(&self, envelope: &Self::Envelope) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + envelope.payload.len());

        // Fixed header: [correlation_id:8][len:4][payload:N]
        buf.extend_from_slice(&envelope.correlation_id.to_le_bytes());
        buf.extend_from_slice(&(envelope.payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&envelope.payload);

        buf
    }

    fn deserialize(&self, data: &[u8]) -> Result<Self::Envelope, SerializationError> {
        if data.len() < 12 {
            return Err(SerializationError::TooShort);
        }

        let correlation_id = u64::from_le_bytes(
            data[0..8]
                .try_into()
                .map_err(|_| SerializationError::InvalidFormat)?,
        );
        let payload_len = u32::from_le_bytes(
            data[8..12]
                .try_into()
                .map_err(|_| SerializationError::InvalidFormat)?,
        ) as usize;

        if data.len() < 12 + payload_len {
            return Err(SerializationError::TooShort);
        }

        Ok(RequestResponseEnvelope {
            correlation_id,
            payload: data[12..12 + payload_len].to_vec(),
        })
    }
}

impl EnvelopeFactory<RequestResponseSerializer> for RequestResponseEnvelope {
    fn create_request(correlation_id: u64, payload: Vec<u8>) -> Self {
        Self {
            correlation_id,
            payload,
        }
    }

    fn create_reply(request: &Self, payload: Vec<u8>) -> Self {
        Self {
            correlation_id: request.correlation_id,
            payload,
        }
    }

    fn extract_payload(envelope: &Self) -> &[u8] {
        &envelope.payload
    }
}

impl EnvelopeReplyDetection for RequestResponseEnvelope {
    fn is_reply_to(&self, correlation_id: u64) -> bool {
        self.correlation_id == correlation_id
    }

    fn correlation_id(&self) -> Option<u64> {
        Some(self.correlation_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_serialization_roundtrip() {
        let serializer = RequestResponseSerializer;
        let envelope = RequestResponseEnvelope {
            correlation_id: 0x1234567890abcdef,
            payload: b"Hello, World!".to_vec(),
        };

        let serialized = serializer.serialize(&envelope);
        let deserialized = serializer
            .deserialize(&serialized)
            .expect("should deserialize");

        assert_eq!(envelope.correlation_id, deserialized.correlation_id);
        assert_eq!(envelope.payload, deserialized.payload);
    }

    #[test]
    fn test_envelope_deserialization_too_short() {
        let serializer = RequestResponseSerializer;
        let short_data = vec![0; 10]; // Less than 12 bytes

        assert_eq!(
            serializer.deserialize(&short_data),
            Err(SerializationError::TooShort)
        );
    }

    #[test]
    fn test_envelope_deserialization_payload_too_short() {
        let serializer = RequestResponseSerializer;
        let mut data = Vec::new();
        data.extend_from_slice(&42u64.to_le_bytes()); // correlation_id
        data.extend_from_slice(&10u32.to_le_bytes()); // payload len = 10
        data.extend_from_slice(&[1, 2, 3, 4, 5]); // only 5 bytes payload

        assert_eq!(
            serializer.deserialize(&data),
            Err(SerializationError::TooShort)
        );
    }
}
