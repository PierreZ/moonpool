//! File storage abstraction for simulation and real file I/O.
//!
//! Two traits and one options type carry every file operation moonpool knows
//! about:
//!
//! - [`StorageProvider`] owns the filesystem *namespace*: opening a path,
//!   asking whether it exists, deleting, renaming.
//! - [`StorageFile`] is one already-open file: stream I/O
//!   ([`AsyncRead`]/[`AsyncWrite`]/[`AsyncSeek`]), positioned I/O
//!   ([`read_at`](StorageFile::read_at) / [`write_at`](StorageFile::write_at)),
//!   durability ([`sync_all`](StorageFile::sync_all) /
//!   [`sync_data`](StorageFile::sync_data)), and size
//!   ([`size`](StorageFile::size) / [`set_len`](StorageFile::set_len)).
//! - [`OpenOptions`] decides the properties of the open.
//!
//! ## One file abstraction, not several
//!
//! A database's needs — positioned reads and writes, uncached I/O, alignment,
//! an explicit durability barrier — are *properties of opening an ordinary
//! file*, not a reason for a parallel provider stack. There is deliberately no
//! `BlockProvider`, no `DirectIoProvider`, and no `DatabaseStorageProvider`:
//! a journal, a pager, or a B-tree is written directly against
//! [`StorageFile`], and conveniences layered on top (such as
//! [`BlockFile`](crate::BlockFile)) wrap an already-open file rather than
//! opening one themselves.
//!
//! ## One file state
//!
//! Every access path reaches the same bytes. A positioned write is visible to
//! a subsequent stream read, a stream write is visible to a subsequent
//! positioned read, and two handles on one path observe one file. The
//! simulation upholds this the same way a real filesystem does: there is one
//! authoritative image per file, never one per API.
//!
//! [`AsyncRead`]: futures::io::AsyncRead
//! [`AsyncWrite`]: futures::io::AsyncWrite
//! [`AsyncSeek`]: futures::io::AsyncSeek

mod align;
mod options;
#[cfg(feature = "tokio-fs")]
mod tokio_impl;

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use std::io;

pub use align::{AlignedBuf, IoConstraints};
pub use options::{DirectIo, OpenOptions};

/// The error a file with I/O constraints returns from the stream API.
///
/// Shared so every backend — and every out-of-tree [`StorageFile`] — refuses
/// it identically, rather than each producing its own wording or, worse,
/// leaving the kernel to produce one.
///
/// See [`StorageFile`] for why the stream API and alignment do not mix.
#[must_use]
pub fn stream_io_unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "this file has I/O alignment constraints, which the stream API cannot honour; \
         use read_at and write_at",
    )
}
#[cfg(feature = "tokio-fs")]
pub use tokio_impl::{TokioStorageFile, TokioStorageProvider};

/// Provider trait for file storage operations.
///
/// The provider owns the filesystem *namespace*: everything that names a path
/// lives here, and everything that operates on already-open bytes lives on
/// [`StorageFile`].
///
/// Clone allows sharing providers across multiple components efficiently.
pub trait StorageProvider: Clone + Send + Sync + 'static {
    /// The file type for this provider.
    type File: StorageFile + 'static;

    /// Open a file with the given options.
    ///
    /// The options decide the file's properties, direct I/O included: a
    /// database file is an ordinary file opened differently, never a different
    /// kind of file. An open that asks for
    /// [`DirectIo::Required`](crate::DirectIo::Required) and cannot get it
    /// fails rather than quietly returning a buffered file.
    fn open(
        &self,
        path: &str,
        options: OpenOptions,
    ) -> impl std::future::Future<Output = io::Result<Self::File>> + Send;

    /// Check if a file exists at the given path.
    fn exists(&self, path: &str) -> impl std::future::Future<Output = io::Result<bool>> + Send;

    /// Delete a file at the given path.
    fn delete(&self, path: &str) -> impl std::future::Future<Output = io::Result<()>> + Send;

    /// Rename a file from one path to another.
    fn rename(
        &self,
        from: &str,
        to: &str,
    ) -> impl std::future::Future<Output = io::Result<()>> + Send;

    /// Make the *directory entries* under `path` durable.
    ///
    /// File durability and directory-entry durability are two different
    /// things, and this is the second one. Syncing a file makes its bytes and
    /// its length survive a crash; it says nothing about whether the *name*
    /// pointing at it does. After
    ///
    /// ```text
    /// open("db/wal", create)   // creates a directory entry
    /// write(..)                // fills the file
    /// sync_all(file)           // the bytes are durable
    /// ```
    ///
    /// a crash may still leave no `db/wal` at all: the entry that names it was
    /// never synced. The engines that get this right (`SQLite`, `PostgreSQL`,
    /// `LMDB`, …) follow a create, a delete, or a rename with a directory
    /// sync, and this is the call that expresses it.
    ///
    /// It lives on the provider, not on [`StorageFile`], because it concerns
    /// the filesystem namespace rather than the contents of an already-open
    /// file — the same reason `rename` and `delete` live here.
    fn sync_dir(&self, path: &str) -> impl std::future::Future<Output = io::Result<()>> + Send;
}

/// Trait for file handles that support async read/write/seek operations.
///
/// ## Durability
///
/// A completed write is *visible*, not *durable*: subsequent reads observe it,
/// but only a completed [`sync_all`](Self::sync_all) or
/// [`sync_data`](Self::sync_data) makes it survive a crash. Nothing else on
/// this trait implies durability.
///
/// ## Stream I/O and alignment do not mix
///
/// The [`AsyncRead`] / [`AsyncWrite`] half of this trait is available only on
/// a file with no I/O constraints. A file that has them — a direct-I/O file —
/// **must** refuse stream reads and writes with
/// [`stream_io_unsupported`], because the stream API cannot honour an
/// alignment: it transfers from wherever the shared cursor happens to be, in
/// whatever length the caller passed, and a short transfer then leaves the
/// cursor somewhere no subsequent request may start from. There is no answer
/// to that within the API's own semantics, so the honest contract is that the
/// two are mutually exclusive rather than one accidentally working until it
/// does not.
///
/// [`read_at`](Self::read_at) and [`write_at`](Self::write_at) are what such a
/// file offers instead, and they are what a storage engine wants anyway.
/// Seeking stays available: it moves a cursor, it does not transfer anything.
pub trait StorageFile: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + Sync + 'static {
    /// Flush this file's data *and* metadata to the device.
    ///
    /// On success every write completed before the call is durable, as is the
    /// file's length. It says nothing about the file's *name*: a directory
    /// entry is made durable by
    /// [`StorageProvider`]-level directory synchronization, not by syncing the
    /// file it points at.
    fn sync_all(&self) -> impl std::future::Future<Output = io::Result<()>> + Send;

    /// Flush this file's data to the device; metadata may lag.
    ///
    /// The cheaper barrier, for the common case where the caller already knows
    /// the file is large enough and only needs the bytes to land.
    fn sync_data(&self) -> impl std::future::Future<Output = io::Result<()>> + Send;

    /// The alignment this file requires of offsets, transfer lengths, and
    /// buffer addresses.
    ///
    /// [`IoConstraints::NONE`] for an ordinary buffered file. For a direct-I/O
    /// file it is what this backend requires of the caller — never less than
    /// the device needs, and possibly more where the backend cannot discover
    /// the device's exact requirement. See [`IoConstraints`] for what that
    /// promises. Cheap: the constraints are fixed when the file is opened.
    fn constraints(&self) -> IoConstraints;

    /// Whether this file is actually doing direct (uncached) I/O.
    ///
    /// Answers what the open *achieved*, not what it asked for — the
    /// difference that matters after
    /// [`DirectIo::Optional`](crate::DirectIo::Optional) fell back.
    fn is_direct_io(&self) -> bool;

    /// Read into `buf` starting at `offset`, without touching the stream
    /// cursor.
    ///
    /// Returns the number of bytes read, which may be **fewer** than
    /// `buf.len()`: this is `read`, not `read_exact`. A return of `0` means
    /// end of file. Positioned reads take `&self`, so non-overlapping ranges
    /// of one file may be read concurrently, and they observe the same bytes
    /// stream reads do.
    ///
    /// A caller that needs the whole range must loop, or use a layer that
    /// loops for it (see [`BlockFile`](crate::BlockFile)).
    fn read_at(
        &self,
        offset: u64,
        buf: &mut [u8],
    ) -> impl std::future::Future<Output = io::Result<usize>> + Send;

    /// Write `buf` starting at `offset`, without touching the stream cursor.
    ///
    /// Returns the number of bytes written, which may be **fewer** than
    /// `buf.len()`: this is `write`, not `write_all`. Writing past the end of
    /// the file extends it. Append mode does not apply — a positioned write
    /// goes exactly where it is told.
    ///
    /// Completion means the bytes are *visible*, not durable; only a
    /// subsequent [`sync_all`](Self::sync_all) / [`sync_data`](Self::sync_data)
    /// makes them survive a crash.
    fn write_at(
        &self,
        offset: u64,
        buf: &[u8],
    ) -> impl std::future::Future<Output = io::Result<usize>> + Send;

    /// Get the current size of the file in bytes.
    fn size(&self) -> impl std::future::Future<Output = io::Result<u64>> + Send;

    /// Set the length of the file, growing or shrinking it.
    ///
    /// Growing a file makes the new range readable; it does **not** allocate
    /// physical storage, initialize the range, or make the new length durable.
    /// Those are three separate concerns, and this method is only the first.
    fn set_len(&self, size: u64) -> impl std::future::Future<Output = io::Result<()>> + Send;
}
