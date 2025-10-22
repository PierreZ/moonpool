//! State serialization abstraction.

use crate::storage::error::StorageError;
use serde::{Deserialize, Serialize};

/// Trait for serializing and deserializing actor state.
///
/// This abstraction allows different serialization formats (JSON, bincode, etc.)
/// to be used for actor state persistence.
pub trait StateSerializer {
    /// Serialize state to bytes.
    ///
    /// # Type Parameters
    ///
    /// - `T`: The state type (must implement Serialize)
    ///
    /// # Returns
    ///
    /// - `Ok(bytes)`: Serialized state data
    /// - `Err(StorageError)`: Serialization failed
    fn serialize<T: Serialize>(&self, state: &T) -> Result<Vec<u8>, StorageError>;

    /// Deserialize state from bytes.
    ///
    /// # Type Parameters
    ///
    /// - `T`: The state type (must implement Deserialize)
    ///
    /// # Returns
    ///
    /// - `Ok(state)`: Deserialized state
    /// - `Err(StorageError)`: Deserialization failed
    fn deserialize<T: for<'de> Deserialize<'de>>(&self, data: &[u8]) -> Result<T, StorageError>;
}

/// JSON-based state serializer using serde_json.
///
/// This is the default serializer implementation, providing human-readable
/// JSON serialization for debugging and inspection.
#[derive(Debug, Clone, Default)]
pub struct JsonSerializer;

impl JsonSerializer {
    /// Create a new JSON serializer.
    pub fn new() -> Self {
        Self
    }
}

impl StateSerializer for JsonSerializer {
    fn serialize<T: Serialize>(&self, state: &T) -> Result<Vec<u8>, StorageError> {
        serde_json::to_vec(state)
            .map_err(|e| StorageError::SerializationFailed(format!("JSON error: {}", e)))
    }

    fn deserialize<T: for<'de> Deserialize<'de>>(&self, data: &[u8]) -> Result<T, StorageError> {
        serde_json::from_slice(data)
            .map_err(|e| StorageError::DeserializationFailed(format!("JSON error: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestState {
        counter: i32,
        name: String,
    }

    #[test]
    fn test_json_serializer_roundtrip() {
        let serializer = JsonSerializer::new();
        let original = TestState {
            counter: 42,
            name: "test".to_string(),
        };

        let bytes = serializer.serialize(&original).unwrap();
        let deserialized: TestState = serializer.deserialize(&bytes).unwrap();

        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_json_serializer_invalid_data() {
        let serializer = JsonSerializer::new();
        let invalid_data = b"not valid json";

        let result: Result<TestState, _> = serializer.deserialize(invalid_data);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            StorageError::DeserializationFailed(_)
        ));
    }
}
