//! Storage error types for simulation operations.

use crate::sim::state::FileId;
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
    #[error("invalid file handle: {file_id:?}")]
    InvalidFileHandle {
        /// The invalid file handle.
        file_id: FileId,
    },

    /// File has been closed.
    #[error("file is closed: {file_id:?}")]
    FileClosed {
        /// The closed file handle.
        file_id: FileId,
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
            StorageError::NotFound { .. } => io::ErrorKind::NotFound,
            StorageError::AlreadyExists { .. } => io::ErrorKind::AlreadyExists,
            StorageError::InvalidFileHandle { .. } => io::ErrorKind::NotFound,
            StorageError::FileClosed { .. } => io::ErrorKind::BrokenPipe,
            StorageError::Io { kind, .. } => *kind,
        };
        io::Error::new(kind, e.to_string())
    }
}
