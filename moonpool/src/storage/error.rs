//! Storage error types.

use std::fmt;

/// Errors that can occur during storage operations.
#[derive(Debug, Clone)]
pub enum StorageError {
    /// Failed to serialize state data.
    SerializationFailed(String),

    /// Failed to deserialize state data.
    DeserializationFailed(String),

    /// Failed to load state from storage backend.
    LoadFailed(String),

    /// Failed to save state to storage backend.
    SaveFailed(String),

    /// Actor state not found in storage.
    NotFound,

    /// Generic storage error.
    Other(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::SerializationFailed(msg) => {
                write!(f, "Serialization failed: {}", msg)
            }
            StorageError::DeserializationFailed(msg) => {
                write!(f, "Deserialization failed: {}", msg)
            }
            StorageError::LoadFailed(msg) => write!(f, "Load failed: {}", msg),
            StorageError::SaveFailed(msg) => write!(f, "Save failed: {}", msg),
            StorageError::NotFound => write!(f, "Actor state not found"),
            StorageError::Other(msg) => write!(f, "Storage error: {}", msg),
        }
    }
}

impl std::error::Error for StorageError {}
