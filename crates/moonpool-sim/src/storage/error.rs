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

    /// A path component that must be a directory is a regular file.
    #[error("not a directory: {path}")]
    NotADirectory {
        /// The offending component.
        path: String,
    },

    /// A file operation was attempted on a directory.
    #[error("is a directory: {path}")]
    IsADirectory {
        /// The directory path.
        path: String,
    },

    /// The path contains bytes the production filesystem rejects.
    #[error("invalid path: {path}")]
    InvalidPath {
        /// The invalid path.
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

    /// The open's flags contradict each other (a read-only truncate, a
    /// create without write access, an append that truncates).
    ///
    /// Refused before the namespace is touched, exactly as `std` refuses it
    /// before reaching the operating system, so the simulator never honors an
    /// open production rejects — nor destroys bytes on the way.
    #[error("invalid open options: {reason}")]
    InvalidOpenOptions {
        /// Which rule the flags broke.
        reason: String,
    },

    /// An open asked for direct I/O the disk cannot provide.
    #[error("direct I/O is not supported by this disk")]
    DirectIoUnsupported,

    /// A `DirectIo::Required` open would have had to create the file.
    ///
    /// `Required` opens an existing file; bootstrapping one is the caller's
    /// protocol, not the provider's. Mirrors the production provider so
    /// simulated code cannot rely on an open production refuses.
    #[error(
        "'{path}' does not exist: direct I/O cannot be required of a file this open would have \
         to create. Create it with an ordinary open, sync it (and its directory, if the name \
         must be durable), then reopen it with DirectIo::Required."
    )]
    DirectIoCreate {
        /// The path the open would have created.
        path: String,
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

    /// An in-flight namespace operation (open, delete, rename,
    /// `create_dir_all`, `sync_dir`) was interrupted by a simulated process
    /// crash or wipe before it took effect.
    #[error("namespace operation {operation_id:?} was interrupted")]
    NamespaceInterrupted {
        /// The interrupted operation.
        operation_id: OperationId,
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

    /// A targeted injection named bytes past the end of the file's durable
    /// image.
    #[error("bytes up to {end} of '{path}' are outside its {len} durable bytes")]
    OutOfRange {
        /// The file's path.
        path: String,
        /// The end of the requested byte range.
        end: u64,
        /// The file's durable length.
        len: u64,
    },

    /// A simulated OS error, with the code that the kernel returns: an
    /// injected fault is [`EIO`] and a full disk is [`ENOSPC`]. It converts
    /// with [`io::Error::from_raw_os_error`], so `raw_os_error()` and
    /// `kind()` are the ones that a real failure gives.
    #[error("OS error {code} on {file_id:?}: {message}")]
    Os {
        /// The file that encountered the error.
        file_id: FileId,
        /// The OS error code.
        code: i32,
        /// What failed.
        message: String,
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

/// The `EIO` code (Linux and macOS): an I/O error of the device.
pub const EIO: i32 = 5;

/// The `ENOSPC` code (Linux and macOS): no space left on the device.
pub const ENOSPC: i32 = 28;

impl From<StorageError> for io::Error {
    fn from(e: StorageError) -> Self {
        if let StorageError::Os { code, .. } = &e {
            return io::Error::from_raw_os_error(*code);
        }
        let kind = match &e {
            StorageError::NotFound { .. } | StorageError::MissingFile { .. } => {
                io::ErrorKind::NotFound
            }
            StorageError::AlreadyExists { .. } => io::ErrorKind::AlreadyExists,
            StorageError::NotADirectory { .. } => io::ErrorKind::NotADirectory,
            StorageError::IsADirectory { .. } => io::ErrorKind::IsADirectory,
            // A crash's interruption is never `Interrupted`: a retry helper
            // would take it for a transient `EINTR`.
            StorageError::InvalidFileHandle { .. }
            | StorageError::FileClosed { .. }
            | StorageError::OperationInterrupted { .. }
            | StorageError::NamespaceInterrupted { .. }
            | StorageError::SimulationShutdown { .. } => io::ErrorKind::BrokenPipe,
            StorageError::PermissionDenied { .. } => io::ErrorKind::PermissionDenied,
            StorageError::InvalidOperation { .. }
            | StorageError::InvalidOperationData { .. }
            | StorageError::InvalidOpenOptions { .. }
            | StorageError::InvalidPath { .. }
            | StorageError::OutOfRange { .. } => io::ErrorKind::InvalidInput,
            StorageError::DirectIoUnsupported | StorageError::DirectIoCreate { .. } => {
                io::ErrorKind::Unsupported
            }

            StorageError::ScheduleFailed { .. } | StorageError::Os { .. } => io::ErrorKind::Other,
            StorageError::Io { kind, .. } => *kind,
        };
        io::Error::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{EIO, ENOSPC, FileId, StorageError};

    #[test]
    fn an_injected_error_has_the_os_code_of_a_real_one() {
        let eio: std::io::Error = StorageError::Os {
            file_id: FileId(1),
            code: EIO,
            message: "read failed".to_string(),
        }
        .into();
        assert_eq!(eio.raw_os_error(), Some(EIO));
        assert_eq!(eio.kind(), std::io::Error::from_raw_os_error(EIO).kind());
        let full: std::io::Error = StorageError::Os {
            file_id: FileId(1),
            code: ENOSPC,
            message: "disk full".to_string(),
        }
        .into();
        assert_eq!(full.raw_os_error(), Some(ENOSPC));
        #[cfg(unix)]
        assert_eq!(full.kind(), std::io::ErrorKind::StorageFull);
    }
}
