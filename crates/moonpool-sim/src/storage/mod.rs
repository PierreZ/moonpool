//! Storage simulation and configuration.
//!
//! This module provides simulated storage that integrates with the
//! deterministic simulation engine for testing disk I/O patterns and faults.
//!
//! There is exactly one simulated file implementation ([`image::FileImage`])
//! and one engine driving it. Every API a caller can reach a file through —
//! stream I/O, positioned I/O, sync, truncation — lands on those bytes, and
//! every fault names a file and a flat sector offset inside it. Layers that
//! give the bytes meaning (a block layer, a pager, a journal) sit above the
//! file provider and inherit its fault model rather than bringing their own.

/// Storage configuration and settings
pub mod config;

/// Storage error types
pub mod error;

/// Storage operation events
pub mod events;

/// Storage file implementation
pub mod file;

/// Future types for async storage operations
pub mod futures;

/// Observable fault vocabulary: kinds, records, crash reports, the
/// eligibility mask.
pub mod faults;

/// The authoritative image of one simulated file: two-image durability, the
/// barrier-bounded crash model, and deterministic damage.
pub mod image;

/// Storage provider implementation
pub mod provider;

/// Deterministic storage engine and targeted event types.
pub mod sim;

use std::io;

/// Create an `io::Error` for simulation shutdown.
///
/// Used by storage futures and file operations when the simulation has been dropped.
pub(crate) fn sim_shutdown_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown")
}

// Re-export error
pub use error::StorageError;

// Re-export configuration
pub use config::StorageConfiguration;

// Re-export events
pub use events::StorageOperation;

// Re-export file
pub use file::SimStorageFile;

// Re-export the file image and the fault vocabulary
pub use faults::{
    CrashOutcome, EioTarget, FileCrashReport, SECTOR_SIZE, SectorResolution,
    StorageEligibilityMask, StorageFaultKind, StorageFaultRecord,
};
pub use image::{FileImage, SectorBitSet};

// Re-export provider
pub use provider::SimStorageProvider;
