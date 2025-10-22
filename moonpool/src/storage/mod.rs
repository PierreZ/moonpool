//! Actor state persistence.

pub mod traits;
pub mod serializer;
pub mod memory;
pub mod error;

// Re-exports (placeholders for now, will be implemented in Phase 7)
// pub use traits::StorageProvider;
// pub use serializer::{JsonSerializer, StateSerializer};
// pub use memory::InMemoryStorage;
