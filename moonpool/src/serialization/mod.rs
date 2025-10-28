//! Serialization abstraction for network messages and actor state.
//!
//! This module provides pluggable serialization for two distinct contexts:
//!
//! - **MessageSerializer**: For serializing network messages (ephemeral, latency-optimized)
//! - **StateSerializer**: For serializing actor state (durable, storage-optimized)
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────┐
//! │ Serializer (base trait)              │
//! │  - serialize<T>(value) -> Vec<u8>    │
//! │  - deserialize<T>(bytes) -> T        │
//! └──────────────────────────────────────┘
//!          ▲                     ▲
//!          │                     │
//! ┌────────┴─────────┐  ┌────────┴─────────┐
//! │ MessageSerializer│  │ StateSerializer  │
//! │ (marker trait)   │  │ (marker trait)   │
//! └──────────────────┘  └──────────────────┘
//! ```
//!
//! # Single-Threaded Design
//!
//! Following moonpool's single-threaded architecture:
//! - No `Send + Sync` bounds (not needed for single-core execution)
//! - Use `Rc<dyn Serializer>` not `Arc<dyn Serializer>`
//! - Simple mutable state (no atomics)
//!
//! # Usage
//!
//! ```rust,ignore
//! use moonpool::serialization::{JsonSerializer, MessageSerializer};
//! use std::rc::Rc;
//!
//! let serializer: Rc<dyn MessageSerializer> = Rc::new(JsonSerializer);
//!
//! #[derive(Serialize, Deserialize)]
//! struct Ping { count: u64 }
//!
//! let ping = Ping { count: 42 };
//! let bytes = serializer.serialize(&ping)?;
//! let decoded: Ping = serializer.deserialize(&bytes)?;
//! ```
//!
//! # Default Implementation
//!
//! `JsonSerializer` implements all three traits and is the default:
//! - Human-readable (easy debugging with packet inspection)
//! - Wide compatibility (every language has JSON)
//! - Good performance (serde_json is well-optimized)
//!
//! # Custom Implementations
//!
//! Users can provide their own serializers:
//!
//! ```rust,ignore
//! struct MsgPackSerializer;
//!
//! impl Serializer for MsgPackSerializer {
//!     fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
//!         rmp_serde::to_vec(value)
//!             .map_err(|e| SerializationError::Custom(e.to_string()))
//!     }
//!
//!     fn deserialize<T: for<'de> Deserialize<'de>>(&self, bytes: &[u8]) -> Result<T> {
//!         rmp_serde::from_slice(bytes)
//!             .map_err(|e| DeserializationError::Custom(e.to_string()))
//!     }
//! }
//!
//! impl MessageSerializer for MsgPackSerializer {}
//! ```
//!
//! # Future Extensions
//!
//! Marker traits enable future specialization without breaking changes:
//!
//! ```rust,ignore
//! // Phase 13+: Add compression hint for messages
//! pub trait MessageSerializer: Serializer {
//!     fn compress(&self) -> bool { false }  // Default: no compression
//! }
//!
//! // Phase 14+: Add schema versioning for state
//! pub trait StateSerializer: Serializer {
//!     fn schema_version(&self) -> u32 { 1 }  // Default: version 1
//! }
//! ```

use serde::{Deserialize, Serialize};

/// Result type for serialization operations.
pub type Result<T> = std::result::Result<T, SerializationError>;

/// Errors that can occur during serialization.
#[derive(Debug, thiserror::Error)]
pub enum SerializationError {
    #[error("Serialization failed: {0}")]
    SerializationFailed(String),

    #[error("Deserialization failed: {0}")]
    DeserializationFailed(String),

    #[error("Custom serialization error: {0}")]
    Custom(String),
}

/// Base serialization trait.
///
/// Provides methods for converting between Rust values and bytes.
/// No `Send + Sync` bounds - follows moonpool's single-threaded design.
///
/// # Type Parameters
///
/// - Uses generic `T` with serde bounds for type-safe (de)serialization
/// - All implementations must handle any `T: Serialize + DeserializeOwned`
///
/// # Error Handling
///
/// Returns `Result<T, SerializationError>` for all operations.
/// Implementations should wrap underlying errors (JSON, bincode, etc.)
/// in `SerializationError` variants.
pub trait Serializer: Clone {
    /// Serialize a value to bytes.
    ///
    /// # Type Parameters
    ///
    /// - `T`: Any type implementing `Serialize`
    ///
    /// # Returns
    ///
    /// - `Ok(Vec<u8>)`: Serialized bytes
    /// - `Err(SerializationError)`: Serialization failed
    fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>>;

    /// Deserialize bytes to a value.
    ///
    /// # Type Parameters
    ///
    /// - `T`: Any type implementing `DeserializeOwned`
    ///
    /// # Returns
    ///
    /// - `Ok(T)`: Deserialized value
    /// - `Err(SerializationError)`: Deserialization failed
    fn deserialize<T: for<'de> Deserialize<'de>>(&self, bytes: &[u8]) -> Result<T>;
}

/// Type-erased serializer for concrete Message type only.
///
/// MessageBus only serializes/deserializes Message envelopes, not arbitrary types.
/// This trait allows MessageBus to avoid being generic by working with the concrete
/// Message type instead of generic `<T>`.
///
/// # Why Type Erasure?
///
/// The `Serializer` trait has generic methods, so it cannot be used as a trait object
/// (`dyn Serializer`). However, MessageBus only needs to serialize one concrete type:
/// `Message`. By creating this trait with concrete methods, we can type-erase the
/// serializer and avoid making MessageBus generic.
///
/// # Example
///
/// ```rust,ignore
/// // Create erased serializer from any Serializer implementation
/// let erased = erase_message_serializer(JsonSerializer);
///
/// // Now can use without knowing concrete type
/// let bytes = erased.serialize_message(&message)?;
/// ```
pub trait ErasedMessageSerializer: 'static {
    /// Serialize a Message to bytes.
    fn serialize_message(&self, msg: &crate::messaging::Message) -> Result<Vec<u8>>;

    /// Deserialize bytes to a Message.
    fn deserialize_message(&self, bytes: &[u8]) -> Result<crate::messaging::Message>;
}

/// Wrapper that implements ErasedMessageSerializer for any Serializer.
///
/// This is the "eraser" that converts `Serializer` (with generic methods)
/// to `ErasedMessageSerializer` (with concrete methods for Message type).
struct MessageSerializerEraser<S>(S);

impl<S: Serializer + 'static> ErasedMessageSerializer for MessageSerializerEraser<S> {
    fn serialize_message(&self, msg: &crate::messaging::Message) -> Result<Vec<u8>> {
        // Call generic method with concrete Message type
        self.0.serialize(msg)
    }

    fn deserialize_message(&self, bytes: &[u8]) -> Result<crate::messaging::Message> {
        // Call generic method with concrete Message type
        self.0.deserialize(bytes)
    }
}

/// Erase a Serializer to ErasedMessageSerializer.
///
/// This function converts any `Serializer` implementation into a trait object
/// that only knows about the Message type.
///
/// # Example
///
/// ```rust,ignore
/// let json_serializer = JsonSerializer;
/// let erased: Box<dyn ErasedMessageSerializer> =
///     erase_message_serializer(json_serializer);
/// ```
pub fn erase_message_serializer<S: Serializer + 'static>(
    serializer: S,
) -> Box<dyn ErasedMessageSerializer> {
    Box::new(MessageSerializerEraser(serializer))
}

/// JSON serializer using serde_json.
///
/// Default implementation that works for both message and state serialization.
///
/// # Characteristics
///
/// - **Human-readable**: Easy debugging (tcpdump, log inspection)
/// - **Cross-language**: Every language has JSON support
/// - **Well-tested**: serde_json is battle-tested
/// - **Good performance**: Optimized for common cases
///
/// # Limitations
///
/// - **Larger than binary formats**: ~2-5x bigger than bincode/msgpack
/// - **Slower than binary formats**: Parsing overhead vs binary formats
/// - **No schema evolution**: Breaking changes require coordination
///
/// For production systems, consider:
/// - MessagePack for network (smaller, faster)
/// - Bincode for state (smallest, fastest for Rust ↔ Rust)
#[derive(Debug, Clone, Default)]
pub struct JsonSerializer;

impl JsonSerializer {
    /// Create a new JSON serializer.
    pub fn new() -> Self {
        Self
    }
}

impl Serializer for JsonSerializer {
    fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
        serde_json::to_vec(value)
            .map_err(|e| SerializationError::SerializationFailed(format!("JSON error: {}", e)))
    }

    fn deserialize<T: for<'de> Deserialize<'de>>(&self, bytes: &[u8]) -> Result<T> {
        serde_json::from_slice(bytes)
            .map_err(|e| SerializationError::DeserializationFailed(format!("JSON error: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestMessage {
        id: u64,
        name: String,
    }

    #[test]
    fn test_json_serializer_roundtrip() {
        let serializer = JsonSerializer::new();
        let original = TestMessage {
            id: 42,
            name: "test".to_string(),
        };

        let bytes = serializer.serialize(&original).unwrap();
        let deserialized: TestMessage = serializer.deserialize(&bytes).unwrap();

        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_json_serializer_invalid_data() {
        let serializer = JsonSerializer::new();
        let invalid_data = b"not valid json";

        let result: Result<TestMessage> = serializer.deserialize(invalid_data);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SerializationError::DeserializationFailed(_)
        ));
    }

    #[test]
    fn test_json_serializer_clone() {
        let serializer1 = JsonSerializer;
        let serializer2 = serializer1.clone();

        let msg = TestMessage {
            id: 42,
            name: "clone test".to_string(),
        };

        let bytes1 = serializer1.serialize(&msg).unwrap();
        let bytes2 = serializer2.serialize(&msg).unwrap();
        assert_eq!(bytes1, bytes2);
    }
}
