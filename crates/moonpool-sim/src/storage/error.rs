//! Storage error types for simulation operations.

use crate::storage::sim::{FileId, HandleId, OperationId};
use std::io;
use thiserror::Error;

/// Errors that can occur during simulated storage operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StorageError {
    /// File not found at the given path.
    #[error("file not found: {path}")]
    NotFound {
        /// The path that was looked up.
        path: String,
    },

    /// File already exists at the given path.
    #[error("file already exists: {path}")]
    AlreadyExists {
        /// The path that already has a file.
        path: String,
    },

    /// File handle is no longer valid (file was deleted or never opened).
    #[error("invalid file handle: {handle_id:?}")]
    InvalidFileHandle {
        /// The invalid file handle.
        handle_id: HandleId,
    },

    /// File has been closed.
    #[error("file is closed: {handle_id:?}")]
    FileClosed {
        /// The closed file handle.
        handle_id: HandleId,
    },

    /// The handle was not opened with permissions required by an operation.
    #[error("{operation} is not permitted for file handle {handle_id:?}")]
    PermissionDenied {
        /// The handle lacking permission.
        handle_id: HandleId,
        /// The operation that was attempted.
        operation: &'static str,
    },

    /// Persistent contents disappeared while a handle was still live.
    #[error("file no longer exists: {file_id:?}")]
    MissingFile {
        /// The missing persistent file.
        file_id: FileId,
    },

    /// A completion or poll referred to an operation the engine does not know.
    #[error("invalid storage operation: {operation_id:?}")]
    InvalidOperation {
        /// The unknown operation.
        operation_id: OperationId,
    },

    /// A pending operation did not carry the data required to complete it.
    #[error("invalid data for storage operation: {operation_id:?}")]
    InvalidOperationData {
        /// The malformed operation.
        operation_id: OperationId,
    },

    /// An in-flight operation was interrupted by a simulated process crash.
    #[error("storage operation {operation_id:?} on {file_id:?} was interrupted")]
    OperationInterrupted {
        /// The interrupted operation.
        operation_id: OperationId,
        /// The persistent file involved.
        file_id: FileId,
    },

    /// An in-flight operation was failed by simulation shutdown.
    #[error("simulation shut down during storage operation {operation_id:?}")]
    SimulationShutdown {
        /// The interrupted operation.
        operation_id: OperationId,
    },

    /// The global scheduler could not accept an operation's completion event.
    #[error("failed to schedule storage operation {operation_id:?}")]
    ScheduleFailed {
        /// The operation whose completion could not be scheduled.
        operation_id: OperationId,
    },

    /// Underlying I/O error from in-memory storage.
    #[error("I/O error on {file_id:?} ({kind:?}): {message}")]
    Io {
        /// The file that encountered the error.
        file_id: FileId,
        /// The I/O error kind.
        kind: io::ErrorKind,
        /// The error message.
        message: String,
    },
}

impl From<StorageError> for io::Error {
    fn from(e: StorageError) -> Self {
        let kind = match &e {
            StorageError::NotFound { .. } | StorageError::MissingFile { .. } => {
                io::ErrorKind::NotFound
            }
            StorageError::AlreadyExists { .. } => io::ErrorKind::AlreadyExists,
            StorageError::InvalidFileHandle { .. } | StorageError::FileClosed { .. } => {
                io::ErrorKind::BrokenPipe
            }
            StorageError::PermissionDenied { .. } => io::ErrorKind::PermissionDenied,
            StorageError::InvalidOperation { .. } | StorageError::InvalidOperationData { .. } => {
                io::ErrorKind::InvalidInput
            }
            StorageError::OperationInterrupted { .. } => io::ErrorKind::Interrupted,
            StorageError::SimulationShutdown { .. } => io::ErrorKind::BrokenPipe,
            StorageError::ScheduleFailed { .. } => io::ErrorKind::Other,
            StorageError::Io { kind, .. } => *kind,
        };
        io::Error::new(kind, e.to_string())
    }
}
