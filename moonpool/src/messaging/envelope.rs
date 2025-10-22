//! Wire protocol envelope for actor messages.
//!
//! Implements binary serialization for efficient over-the-wire transmission
//! of actor messages.

use crate::actor::{ActorId, CorrelationId, NodeId};
use crate::error::MessageError;
use crate::messaging::{Direction, Message, MessageFlags};
use crate::messaging::message::{ActorAddress, CacheUpdate};
use std::io::{Cursor, Read, Write};

/// Maximum message size: 1MB
const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Wire format envelope for actor messages.
///
/// # Binary Format
///
/// ```text
/// [message_type: 1 byte]           // Request=1, Response=2, OneWay=3
/// [correlation_id: 8 bytes (u64)]  // For request-response matching
/// [target_actor_len: 4 bytes (u32)]
/// [target_actor: N bytes (UTF-8)]  // ActorId string format
/// [sender_actor_len: 4 bytes (u32)]
/// [sender_actor: M bytes (UTF-8)]  // ActorId string format
/// [target_node_len: 4 bytes (u32)]
/// [target_node: P bytes (UTF-8)]   // NodeId string
/// [sender_node_len: 4 bytes (u32)]
/// [sender_node: Q bytes (UTF-8)]   // NodeId string
/// [method_name_len: 4 bytes (u32)]
/// [method_name: R bytes (UTF-8)]
/// [flags: 2 bytes (u16)]           // MessageFlags bits
/// [forward_count: 1 byte]
/// [cache_update_count: 4 bytes (u32)]
/// [cache_updates: repeated CacheUpdate]
/// [payload_len: 4 bytes (u32)]
/// [payload: S bytes]               // Application data
/// ```
///
/// # Design Rationale
///
/// - Little-endian byte order (deterministic, efficient)
/// - Length-prefixed strings (safe deserialization)
/// - Compact binary format (reduces network bandwidth)
/// - Streaming-friendly (can parse partial messages)
pub struct ActorEnvelope;

impl ActorEnvelope {
    /// Serialize a message to wire format.
    ///
    /// # Errors
    ///
    /// Returns `MessageError::SerializationFailed` if:
    /// - Message exceeds MAX_MESSAGE_SIZE
    /// - String encoding fails
    /// - I/O error during serialization
    pub fn serialize(message: &Message) -> Result<Vec<u8>, MessageError> {
        let mut buffer = Vec::with_capacity(512); // Typical message size

        // 1. Message type (Direction)
        let msg_type: u8 = match message.direction {
            Direction::Request => 1,
            Direction::Response => 2,
            Direction::OneWay => 3,
        };
        buffer.write_all(&[msg_type])?;

        // 2. Correlation ID (8 bytes, little-endian)
        buffer.write_all(&message.correlation_id.as_u64().to_le_bytes())?;

        // 3. Target actor ID (length-prefixed UTF-8)
        Self::write_string(&mut buffer, &message.target_actor.to_string_format())?;

        // 4. Sender actor ID (length-prefixed UTF-8)
        Self::write_string(&mut buffer, &message.sender_actor.to_string_format())?;

        // 5. Target node ID (length-prefixed UTF-8)
        Self::write_string(&mut buffer, message.target_node.as_str())?;

        // 6. Sender node ID (length-prefixed UTF-8)
        Self::write_string(&mut buffer, message.sender_node.as_str())?;

        // 7. Method name (length-prefixed UTF-8)
        Self::write_string(&mut buffer, &message.method_name)?;

        // 8. Flags (2 bytes, little-endian)
        buffer.write_all(&message.flags.bits().to_le_bytes())?;

        // 9. Forward count (1 byte)
        buffer.write_all(&[message.forward_count])?;

        // 10. Cache updates
        let empty_vec = vec![];
        let cache_updates = message.cache_invalidation.as_ref().unwrap_or(&empty_vec);
        buffer.write_all(&(cache_updates.len() as u32).to_le_bytes())?;

        for update in cache_updates {
            Self::write_cache_update(&mut buffer, update)?;
        }

        // 11. Payload (length-prefixed binary)
        let payload_len = message.payload.len() as u32;
        buffer.write_all(&payload_len.to_le_bytes())?;
        buffer.write_all(&message.payload)?;

        // Check maximum message size
        if buffer.len() > MAX_MESSAGE_SIZE {
            return Err(MessageError::MessageTooLarge {
                size: buffer.len(),
                max: MAX_MESSAGE_SIZE,
            });
        }

        Ok(buffer)
    }

    /// Deserialize a message from wire format.
    ///
    /// # Errors
    ///
    /// Returns `MessageError::DeserializationFailed` if:
    /// - Incomplete message (buffer too short)
    /// - Invalid UTF-8 encoding
    /// - Invalid enum values
    /// - Message exceeds MAX_MESSAGE_SIZE
    pub fn deserialize(data: &[u8]) -> Result<Message, MessageError> {
        if data.len() > MAX_MESSAGE_SIZE {
            return Err(MessageError::MessageTooLarge {
                size: data.len(),
                max: MAX_MESSAGE_SIZE,
            });
        }

        let mut cursor = Cursor::new(data);

        // 1. Message type (Direction)
        let mut buf = [0u8; 1];
        cursor.read_exact(&mut buf)?;
        let direction = match buf[0] {
            1 => Direction::Request,
            2 => Direction::Response,
            3 => Direction::OneWay,
            _ => {
                return Err(MessageError::DeserializationFailed(format!(
                    "Invalid direction: {}",
                    buf[0]
                )))
            }
        };

        // 2. Correlation ID (8 bytes, little-endian)
        let mut buf = [0u8; 8];
        cursor.read_exact(&mut buf)?;
        let correlation_id = CorrelationId::new(u64::from_le_bytes(buf));

        // 3. Target actor ID
        let target_actor_str = Self::read_string(&mut cursor)?;
        let target_actor = ActorId::from_string(&target_actor_str)
            .map_err(|e| MessageError::DeserializationFailed(format!("Invalid ActorId: {}", e)))?;

        // 4. Sender actor ID
        let sender_actor_str = Self::read_string(&mut cursor)?;
        let sender_actor = ActorId::from_string(&sender_actor_str)
            .map_err(|e| MessageError::DeserializationFailed(format!("Invalid ActorId: {}", e)))?;

        // 5. Target node ID
        let target_node_str = Self::read_string(&mut cursor)?;
        let target_node = NodeId::from(target_node_str)
            .map_err(|e| MessageError::DeserializationFailed(format!("Invalid NodeId: {}", e)))?;

        // 6. Sender node ID
        let sender_node_str = Self::read_string(&mut cursor)?;
        let sender_node = NodeId::from(sender_node_str)
            .map_err(|e| MessageError::DeserializationFailed(format!("Invalid NodeId: {}", e)))?;

        // 7. Method name
        let method_name = Self::read_string(&mut cursor)?;

        // 8. Flags (2 bytes, little-endian)
        let mut buf = [0u8; 2];
        cursor.read_exact(&mut buf)?;
        let flags = MessageFlags::from_bits_truncate(u16::from_le_bytes(buf));

        // 9. Forward count (1 byte)
        let mut buf = [0u8; 1];
        cursor.read_exact(&mut buf)?;
        let forward_count = buf[0];

        // 10. Cache updates
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf)?;
        let cache_update_count = u32::from_le_bytes(buf);

        let mut cache_invalidation = Vec::new();
        for _ in 0..cache_update_count {
            cache_invalidation.push(Self::read_cache_update(&mut cursor)?);
        }
        let cache_invalidation = if cache_invalidation.is_empty() {
            None
        } else {
            Some(cache_invalidation)
        };

        // 11. Payload (length-prefixed binary)
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf)?;
        let payload_len = u32::from_le_bytes(buf) as usize;

        let mut payload = vec![0u8; payload_len];
        cursor.read_exact(&mut payload)?;

        // Construct message (time_to_expiry not serialized, will be set by receiver if needed)
        Ok(Message {
            correlation_id,
            direction,
            target_actor,
            sender_actor,
            target_node,
            sender_node,
            method_name,
            payload,
            flags,
            time_to_expiry: None, // Not transmitted over wire
            forward_count,
            cache_invalidation,
        })
    }

    /// Try to deserialize a message, returning number of bytes consumed if successful.
    ///
    /// This is useful for streaming reception where messages may arrive in fragments.
    ///
    /// # Returns
    ///
    /// - `Ok(Some((message, bytes_consumed)))` - Successfully deserialized
    /// - `Ok(None)` - Need more data (incomplete message)
    /// - `Err(MessageError)` - Deserialization failed
    pub fn try_deserialize(data: &[u8]) -> Result<Option<(Message, usize)>, MessageError> {
        // Try to deserialize - if it fails due to incomplete data, return None
        match Self::deserialize(data) {
            Ok(message) => {
                // Calculate how many bytes were consumed
                // We can re-serialize to determine exact size, but for now assume full buffer
                // TODO: Optimize this by tracking cursor position during deserialization
                Ok(Some((message, data.len())))
            }
            Err(MessageError::IoError(_)) => {
                // I/O error likely means incomplete data
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Write a length-prefixed UTF-8 string.
    fn write_string(buffer: &mut Vec<u8>, s: &str) -> Result<(), MessageError> {
        let bytes = s.as_bytes();
        let len = bytes.len() as u32;
        buffer.write_all(&len.to_le_bytes())?;
        buffer.write_all(bytes)?;
        Ok(())
    }

    /// Read a length-prefixed UTF-8 string.
    fn read_string(cursor: &mut Cursor<&[u8]>) -> Result<String, MessageError> {
        let mut buf = [0u8; 4];
        cursor.read_exact(&mut buf)?;
        let len = u32::from_le_bytes(buf) as usize;

        let mut string_bytes = vec![0u8; len];
        cursor.read_exact(&mut string_bytes)?;

        String::from_utf8(string_bytes)
            .map_err(|e| MessageError::DeserializationFailed(format!("Invalid UTF-8: {}", e)))
    }

    /// Write a CacheUpdate.
    fn write_cache_update(buffer: &mut Vec<u8>, update: &CacheUpdate) -> Result<(), MessageError> {
        // Invalid address
        Self::write_actor_address(buffer, &update.invalid_address)?;

        // Valid address (optional)
        match &update.valid_address {
            Some(addr) => {
                buffer.write_all(&[1u8])?; // Has valid address
                Self::write_actor_address(buffer, addr)?;
            }
            None => {
                buffer.write_all(&[0u8])?; // No valid address
            }
        }

        Ok(())
    }

    /// Read a CacheUpdate.
    fn read_cache_update(cursor: &mut Cursor<&[u8]>) -> Result<CacheUpdate, MessageError> {
        // Invalid address
        let invalid_address = Self::read_actor_address(cursor)?;

        // Valid address (optional)
        let mut buf = [0u8; 1];
        cursor.read_exact(&mut buf)?;
        let valid_address = if buf[0] == 1 {
            Some(Self::read_actor_address(cursor)?)
        } else {
            None
        };

        Ok(CacheUpdate {
            invalid_address,
            valid_address,
        })
    }

    /// Write an ActorAddress.
    fn write_actor_address(buffer: &mut Vec<u8>, address: &ActorAddress) -> Result<(), MessageError> {
        Self::write_string(buffer, &address.actor_id.to_string_format())?;
        Self::write_string(buffer, address.node_id.as_str())?;
        Ok(())
    }

    /// Read an ActorAddress.
    fn read_actor_address(cursor: &mut Cursor<&[u8]>) -> Result<ActorAddress, MessageError> {
        let actor_id_str = Self::read_string(cursor)?;
        let actor_id = ActorId::from_string(&actor_id_str)
            .map_err(|e| MessageError::DeserializationFailed(format!("Invalid ActorId: {}", e)))?;

        let node_id_str = Self::read_string(cursor)?;
        let node_id = NodeId::from(node_id_str)
            .map_err(|e| MessageError::DeserializationFailed(format!("Invalid NodeId: {}", e)))?;

        Ok(ActorAddress {
            actor_id,
            node_id,
            activation_time: std::time::Instant::now(), // Not transmitted
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_request_round_trip() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let original = Message::request(
            CorrelationId::new(42),
            target,
            sender,
            target_node,
            sender_node,
            "DepositRequest".to_string(),
            vec![1, 2, 3, 4, 5],
            std::time::Duration::from_secs(30),
        );

        // Serialize
        let wire_bytes = ActorEnvelope::serialize(&original).unwrap();

        // Deserialize
        let deserialized = ActorEnvelope::deserialize(&wire_bytes).unwrap();

        // Verify fields (excluding time_to_expiry which is not serialized)
        assert_eq!(deserialized.correlation_id, original.correlation_id);
        assert_eq!(deserialized.direction, original.direction);
        assert_eq!(deserialized.target_actor, original.target_actor);
        assert_eq!(deserialized.sender_actor, original.sender_actor);
        assert_eq!(deserialized.target_node, original.target_node);
        assert_eq!(deserialized.sender_node, original.sender_node);
        assert_eq!(deserialized.method_name, original.method_name);
        assert_eq!(deserialized.payload, original.payload);
        assert_eq!(deserialized.flags, original.flags);
        assert_eq!(deserialized.forward_count, original.forward_count);
    }

    #[test]
    fn test_envelope_response_round_trip() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let request = Message::request(
            CorrelationId::new(42),
            target,
            sender,
            target_node,
            sender_node,
            "DepositRequest".to_string(),
            vec![1, 2, 3],
            std::time::Duration::from_secs(30),
        );

        let original = Message::response(&request, vec![100, 101, 102]);

        // Serialize
        let wire_bytes = ActorEnvelope::serialize(&original).unwrap();

        // Deserialize
        let deserialized = ActorEnvelope::deserialize(&wire_bytes).unwrap();

        // Verify
        assert_eq!(deserialized.direction, Direction::Response);
        assert_eq!(deserialized.correlation_id, original.correlation_id);
        assert_eq!(deserialized.payload, vec![100, 101, 102]);
    }

    #[test]
    fn test_envelope_oneway_round_trip() {
        let target = ActorId::new("prod", "Logger", "main");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let original = Message::oneway(
            target,
            sender,
            target_node,
            sender_node,
            "LogEvent".to_string(),
            vec![7, 8, 9],
        );

        // Serialize
        let wire_bytes = ActorEnvelope::serialize(&original).unwrap();

        // Deserialize
        let deserialized = ActorEnvelope::deserialize(&wire_bytes).unwrap();

        // Verify
        assert_eq!(deserialized.direction, Direction::OneWay);
        assert_eq!(deserialized.payload, vec![7, 8, 9]);
    }

    #[test]
    fn test_envelope_with_flags() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let mut message = Message::request(
            CorrelationId::new(1),
            target,
            sender,
            target_node,
            sender_node,
            "GetBalance".to_string(),
            vec![],
            std::time::Duration::from_secs(30),
        );
        message.flags = MessageFlags::READ_ONLY | MessageFlags::ALWAYS_INTERLEAVE;

        // Round trip
        let wire_bytes = ActorEnvelope::serialize(&message).unwrap();
        let deserialized = ActorEnvelope::deserialize(&wire_bytes).unwrap();

        assert_eq!(deserialized.flags, message.flags);
    }

    #[test]
    fn test_envelope_max_size_exceeded() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        // Create message with very large payload
        let large_payload = vec![0u8; MAX_MESSAGE_SIZE + 1];
        let message = Message::request(
            CorrelationId::new(1),
            target,
            sender,
            target_node,
            sender_node,
            "LargeRequest".to_string(),
            large_payload,
            std::time::Duration::from_secs(30),
        );

        // Should fail serialization
        let result = ActorEnvelope::serialize(&message);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), MessageError::MessageTooLarge { .. }));
    }

    #[test]
    fn test_envelope_try_deserialize_incomplete() {
        // Create incomplete message (just the message type byte)
        let incomplete_data = vec![1u8];

        // Should return None (need more data)
        let result = ActorEnvelope::try_deserialize(&incomplete_data).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_envelope_with_cache_invalidation() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let mut message = Message::request(
            CorrelationId::new(1),
            target.clone(),
            sender,
            target_node.clone(),
            sender_node,
            "Test".to_string(),
            vec![],
            std::time::Duration::from_secs(30),
        );

        // Add cache invalidation
        message.cache_invalidation = Some(vec![CacheUpdate {
            invalid_address: ActorAddress {
                actor_id: target.clone(),
                node_id: NodeId::from("127.0.0.1:6000").unwrap(),
                activation_time: std::time::Instant::now(),
            },
            valid_address: Some(ActorAddress {
                actor_id: target,
                node_id: target_node,
                activation_time: std::time::Instant::now(),
            }),
        }]);

        // Round trip
        let wire_bytes = ActorEnvelope::serialize(&message).unwrap();
        let deserialized = ActorEnvelope::deserialize(&wire_bytes).unwrap();

        assert!(deserialized.cache_invalidation.is_some());
        let updates = deserialized.cache_invalidation.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].invalid_address.actor_id, message.cache_invalidation.as_ref().unwrap()[0].invalid_address.actor_id);
    }
}
