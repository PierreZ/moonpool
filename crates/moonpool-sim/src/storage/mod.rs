//! Storage simulation: simulated files, their crash model and their faults.
//!
//! Code opens files through [`StorageProvider`](crate::StorageProvider) and
//! uses them through [`StorageFile`](crate::StorageFile). In simulation the
//! provider is a [`SimStorageProvider`], scoped to one process IP, so every
//! file has an owner and every fault can aim at one process.
//!
//! ```rust
//! use moonpool_sim::{OpenOptions, StorageFile, StorageProvider};
//!
//! /// Shipped code: generic over the provider, the same in production and
//! /// in simulation.
//! async fn save<S: StorageProvider>(storage: &S, record: &[u8]) -> std::io::Result<()> {
//!     let file = storage.open("record.dat", OpenOptions::create_write()).await?;
//!     file.write_at(0, record).await?;
//!     // Until this sync returns, a crash may lose or tear the write.
//!     file.sync_all().await
//! }
//! ```
//!
//! # One file image, one engine
//!
//! There is exactly one simulated file implementation ([`FileImage`]) and
//! one engine driving it ([`sim::StorageEngine`]). Every way to reach a file
//! (stream I/O, positioned I/O, sync, truncation) lands on those bytes, and
//! every fault names a file and a flat sector offset inside it. Layers that
//! give the bytes meaning (a block layer such as `moonpool_core::BlockFile`,
//! a pager, a journal) sit above the provider and inherit its fault model.
//!
//! # Crash model
//!
//! A file keeps a committed image (the bytes at the last successful sync)
//! and a visible image (what reads see). At a crash, each sector written
//! since the last sync resolves on its own to old, new, lost or damaged
//! ([`CrashOutcome`]). Nothing synced is ever lost: a lost-synced-write
//! oracle checks that after every crash, unless the opt-in barrier-violation
//! family is armed. A crash returns a [`FileCrashReport`] per file.
//!
//! # Faults
//!
//! [`Chaos::Storage`](crate::Chaos::Storage) samples a
//! [`StorageConfiguration`] per seed. Its fault families ([`StorageFault`])
//! are corruption, `EIO`, misdirected I/O, phantom writes, sync failures,
//! short transfers, directory-entry loss, crash damage, disk stalls and
//! throttles, and disk failure. Every fault that fires is recorded as a
//! [`StorageFaultRecord`] of a [`StorageFaultKind`].
//!
//! To control where faults land:
//!
//! - [`StorageFaultMask`] (through
//!   [`SimulationBuilder::storage_fault_mask`](crate::SimulationBuilder::storage_fault_mask))
//!   removes whole families.
//! - [`FaultFocus`] (through [`SimStorageProvider::focus_faults`]) weighs
//!   random faults toward byte ranges of one disk.
//! - [`ReplicatedFaults`] (through
//!   [`SimulationBuilder::replicated_storage_faults`](crate::SimulationBuilder::replicated_storage_faults))
//!   spreads damage over failure domains, using the
//!   [`LayoutRegion`](crate::LayoutRegion)s a process publishes with
//!   [`SimStorageProvider::publish_layout`].
//! - [`FaultContext`](crate::FaultContext) lets a custom injector corrupt
//!   bytes, fail a file with `EIO`, read a file's durable bytes and give a
//!   disk a [`DiskCapacity`], so it fills up and refuses growth with
//!   `ENOSPC`.

/// Disk capacity: a simulated disk that fills up.
pub mod capacity;

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

// Replicated fault patterns: damage spread over failure domains
pub mod replication;

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

// Re-export the disk capacity
pub use capacity::DiskCapacity;

// Re-export configuration
pub use config::{StorageConfiguration, StorageFault, StorageFaultMask, WritebackErrorReporting};

// Re-export events
pub use events::StorageOperation;

// Re-export file
pub use file::SimStorageFile;

// Re-export the file image and the fault vocabulary
pub use faults::{
    CrashOutcome, EioTarget, FaultFocus, FileCrashReport, SECTOR_SIZE, SectorResolution,
    StorageEligibilityMask, StorageFaultKind, StorageFaultRecord,
};
pub use image::{FileImage, SectorBitSet};

pub use replication::{FaultPattern, ReplicatedFaults};

// Re-export provider
pub use provider::SimStorageProvider;
