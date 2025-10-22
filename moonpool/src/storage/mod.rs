//! Actor state persistence.

pub mod error;
pub mod memory;
pub mod serializer;
pub mod traits;

// Re-exports (placeholders for now, will be implemented in Phase 7)
// pub use traits::StorageProvider;
// pub use serializer::{JsonSerializer, StateSerializer};
// pub use memory::InMemoryStorage;
