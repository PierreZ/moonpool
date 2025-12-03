//! Common utilities for E2E simulation tests.
//!
//! Provides shared infrastructure for client/server workloads,
//! message tracking, and invariant validation.

pub mod invariants;
pub mod operations;
pub mod tests;
pub mod workloads;

use moonpool_transport::UID;

/// Test message format for tracking delivery.
///
/// Each message has a unique sequence ID for tracking:
/// - Which messages were sent
/// - Which messages were received
/// - Message ordering
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestMessage {
    /// Monotonically increasing sequence ID per sender
    pub sequence_id: u64,
    /// Identifier for the sender workload
    pub sender_id: String,
    /// Whether this was sent via reliable queue
    pub sent_reliably: bool,
    /// Optional payload for size testing
    pub payload: Vec<u8>,
}

impl TestMessage {
    /// Create a new test message.
    pub fn new(sequence_id: u64, sender_id: impl Into<String>, sent_reliably: bool) -> Self {
        Self {
            sequence_id,
            sender_id: sender_id.into(),
            sent_reliably,
            payload: Vec::new(),
        }
    }

    /// Create a test message with payload of specified size.
    pub fn with_payload(
        sequence_id: u64,
        sender_id: impl Into<String>,
        sent_reliably: bool,
        payload_size: usize,
    ) -> Self {
        Self {
            sequence_id,
            sender_id: sender_id.into(),
            sent_reliably,
            payload: vec![0xAB; payload_size],
        }
    }

    /// Serialize to bytes for sending over wire.
    ///
    /// Simple format:
    /// - 8 bytes: sequence_id (little-endian u64)
    /// - 1 byte: sender_id length (max 255)
    /// - N bytes: sender_id (UTF-8)
    /// - 1 byte: sent_reliably flag (0 or 1)
    /// - 4 bytes: payload length (little-endian u32)
    /// - N bytes: payload
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // sequence_id
        buf.extend_from_slice(&self.sequence_id.to_le_bytes());

        // sender_id
        let sender_bytes = self.sender_id.as_bytes();
        buf.push(sender_bytes.len() as u8);
        buf.extend_from_slice(sender_bytes);

        // sent_reliably
        buf.push(if self.sent_reliably { 1 } else { 0 });

        // payload
        buf.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.payload);

        buf
    }

    /// Deserialize from bytes received over wire.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() < 14 {
            // Minimum: 8 + 1 + 0 + 1 + 4 = 14 bytes
            return Err("insufficient data");
        }

        let mut pos = 0;

        // sequence_id
        let sequence_id = u64::from_le_bytes(
            bytes[pos..pos + 8]
                .try_into()
                .map_err(|_| "invalid sequence_id")?,
        );
        pos += 8;

        // sender_id
        let sender_len = bytes[pos] as usize;
        pos += 1;

        if pos + sender_len > bytes.len() {
            return Err("truncated sender_id");
        }
        let sender_id =
            std::str::from_utf8(&bytes[pos..pos + sender_len]).map_err(|_| "invalid sender_id")?;
        pos += sender_len;

        // sent_reliably
        if pos >= bytes.len() {
            return Err("missing sent_reliably");
        }
        let sent_reliably = bytes[pos] != 0;
        pos += 1;

        // payload length
        if pos + 4 > bytes.len() {
            return Err("missing payload length");
        }
        let payload_len = u32::from_le_bytes(
            bytes[pos..pos + 4]
                .try_into()
                .map_err(|_| "invalid payload length")?,
        ) as usize;
        pos += 4;

        // payload
        if pos + payload_len > bytes.len() {
            return Err("truncated payload");
        }
        let payload = bytes[pos..pos + payload_len].to_vec();

        Ok(Self {
            sequence_id,
            sender_id: sender_id.to_string(),
            sent_reliably,
            payload,
        })
    }
}

/// Well-known UID token for test messages.
pub fn test_message_token() -> UID {
    UID::well_known(100)
}

/// Well-known UID token for test acknowledgments.
pub fn test_ack_token() -> UID {
    UID::well_known(101)
}

#[cfg(test)]
mod serialization_tests {
    use super::*;

    #[test]
    fn test_message_serialization_roundtrip() {
        let msg = TestMessage::with_payload(42, "client_1", true, 100);
        let bytes = msg.to_bytes();
        let decoded = TestMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_minimal() {
        let msg = TestMessage::new(0, "", false);
        let bytes = msg.to_bytes();
        let decoded = TestMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }
}
