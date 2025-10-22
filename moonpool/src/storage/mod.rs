//! Actor state persistence and storage abstraction.
//!
//! This module provides a pluggable storage system for actor state persistence:
//!
//! - **StorageProvider**: Trait for loading and saving actor state
//! - **StateSerializer**: Trait for serializing actor state to/from bytes
//! - **InMemoryStorage**: Simple in-memory storage implementation for testing

pub mod error;
pub mod memory;
pub mod serializer;
pub mod traits;

// Re-exports
pub use error::StorageError;
pub use memory::InMemoryStorage;
pub use serializer::{JsonSerializer, StateSerializer};
pub use traits::StorageProvider;
