//! Storage simulation and configuration.
//!
//! This module provides simulated storage that integrates with the
//! deterministic simulation engine for testing disk I/O patterns and faults.

/// Storage configuration and settings
pub mod config;

/// Storage operation events
pub mod events;

/// Storage file implementation
pub mod file;

/// Future types for async storage operations
pub mod futures;

/// In-memory storage with deterministic fault injection
pub mod memory;

/// Storage provider implementation
pub mod provider;

use std::io;

/// Create an io::Error for simulation shutdown.
///
/// Used by storage futures and file operations when the simulation has been dropped.
pub(crate) fn sim_shutdown_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown")
}

// Re-export configuration
pub use config::StorageConfiguration;

// Re-export events
pub use events::StorageOperation;

// Re-export file
pub use file::SimStorageFile;

// Re-export memory storage
pub use memory::{InMemoryStorage, SECTOR_SIZE, SectorBitSet};

// Re-export provider
pub use provider::SimStorageProvider;
