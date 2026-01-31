//! Storage operation events for simulation.
//!
//! These events are scheduled by SimStorageFile/SimStorageProvider and
//! processed by SimWorld to simulate storage I/O with deterministic
//! timing and fault injection.

/// Storage operations that can be scheduled in the simulation.
///
/// Unlike network operations which model data delivery, storage operations
/// model the completion of I/O requests. Each operation represents a
/// pending I/O that completes at the scheduled time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageOperation {
    /// Read operation completed.
    ///
    /// The data has been read from storage and is ready to return
    /// to the caller. Faults (if any) are applied when processing.
    ReadComplete {
        /// Number of bytes read
        len: u32,
    },

    /// Write operation completed.
    ///
    /// The data has been accepted by the storage layer. Note that
    /// without a sync, this data may be lost on crash.
    WriteComplete {
        /// Number of bytes written
        len: u32,
    },

    /// Sync operation completed.
    ///
    /// All previously written data is now durable. May fail with
    /// sync_failure_probability.
    SyncComplete,

    /// File open operation completed.
    OpenComplete,

    /// File truncate/extend operation completed.
    SetLenComplete {
        /// New file length
        new_len: u64,
    },
}
