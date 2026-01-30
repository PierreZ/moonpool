//! Storage simulation and configuration.
//!
//! This module provides simulated storage that integrates with the
//! deterministic simulation engine for testing disk I/O patterns and faults.

/// Storage configuration and settings
pub mod config;

/// In-memory storage with deterministic fault injection
pub mod memory;

// Re-export configuration
pub use config::StorageConfiguration;

// Re-export memory storage
pub use memory::{InMemoryStorage, SECTOR_SIZE, SectorBitSet};
