use super::{EnvelopeSerializer, SerializationError};

/// Request-response envelope for correlation (FDB-inspired).
///
/// This envelope format enables request-response correlation by tracking
/// destination and source tokens, allowing responses to be routed back
/// to the correct sender.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestResponseEnvelope {
    /// Where this message should be delivered (destination token)
    pub destination_token: u64,
    /// Where response should be sent back (source token)  
    pub source_token: u64,
    /// Raw application data payload
    pub payload: Vec<u8>,
}

/// Serializer for RequestResponseEnvelope using a fixed binary format.
///
/// Wire format: [dest:8][source:8][len:4][payload:N]
/// - dest: destination token (8 bytes, little-endian u64)
/// - source: source token (8 bytes, little-endian u64)
/// - len: payload length (4 bytes, little-endian u32)
/// - payload: raw payload bytes (N bytes)
#[derive(Clone)]
pub struct RequestResponseSerializer;

impl EnvelopeSerializer for RequestResponseSerializer {
    type Envelope = RequestResponseEnvelope;

    fn serialize(&self, envelope: &Self::Envelope) -> Vec<u8> {
        let mut buf = Vec::with_capacity(20 + envelope.payload.len());

        // Fixed header: [dest:8][source:8][len:4][payload:N]
        buf.extend_from_slice(&envelope.destination_token.to_le_bytes());
        buf.extend_from_slice(&envelope.source_token.to_le_bytes());
        buf.extend_from_slice(&(envelope.payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&envelope.payload);

        buf
    }

    fn deserialize(&self, data: &[u8]) -> Result<Self::Envelope, SerializationError> {
        if data.len() < 20 {
            return Err(SerializationError::TooShort);
        }

        let destination_token = u64::from_le_bytes(
            data[0..8]
                .try_into()
                .map_err(|_| SerializationError::InvalidFormat)?,
        );
        let source_token = u64::from_le_bytes(
            data[8..16]
                .try_into()
                .map_err(|_| SerializationError::InvalidFormat)?,
        );
        let payload_len = u32::from_le_bytes(
            data[16..20]
                .try_into()
                .map_err(|_| SerializationError::InvalidFormat)?,
        ) as usize;

        if data.len() < 20 + payload_len {
            return Err(SerializationError::TooShort);
        }

        Ok(RequestResponseEnvelope {
            destination_token,
            source_token,
            payload: data[20..20 + payload_len].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_serialization_roundtrip() {
        let serializer = RequestResponseSerializer;
        let envelope = RequestResponseEnvelope {
            destination_token: 0x1234567890abcdef,
            source_token: 0xfedcba0987654321,
            payload: b"Hello, World!".to_vec(),
        };

        let serialized = serializer.serialize(&envelope);
        let deserialized = serializer
            .deserialize(&serialized)
            .expect("should deserialize");

        assert_eq!(envelope.destination_token, deserialized.destination_token);
        assert_eq!(envelope.source_token, deserialized.source_token);
        assert_eq!(envelope.payload, deserialized.payload);
    }

    #[test]
    fn test_envelope_deserialization_too_short() {
        let serializer = RequestResponseSerializer;
        let short_data = vec![0; 10]; // Less than 20 bytes

        assert_eq!(
            serializer.deserialize(&short_data),
            Err(SerializationError::TooShort)
        );
    }

    #[test]
    fn test_envelope_deserialization_payload_too_short() {
        let serializer = RequestResponseSerializer;
        let mut data = Vec::new();
        data.extend_from_slice(&42u64.to_le_bytes()); // dest
        data.extend_from_slice(&43u64.to_le_bytes()); // source
        data.extend_from_slice(&10u32.to_le_bytes()); // payload len = 10
        data.extend_from_slice(&[1, 2, 3, 4, 5]); // only 5 bytes payload

        assert_eq!(
            serializer.deserialize(&data),
            Err(SerializationError::TooShort)
        );
    }
}
