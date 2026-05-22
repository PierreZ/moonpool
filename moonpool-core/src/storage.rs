//! Storage provider abstraction for simulation and real file I/O.
//!
//! This module provides trait-based file storage that allows seamless swapping
//! between real Tokio file I/O and simulated storage for testing.

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use std::io;
#[cfg(feature = "tokio-providers")]
use std::io::SeekFrom;
#[cfg(feature = "tokio-providers")]
use std::pin::Pin;
#[cfg(feature = "tokio-providers")]
use std::task::{Context, Poll};
#[cfg(feature = "tokio-providers")]
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

/// Options for opening a file.
///
/// This struct provides a builder-style API for configuring how a file
/// should be opened, similar to [`std::fs::OpenOptions`]. The individual
/// flags are stored in a single bitfield and exposed via `is_*` accessors.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Bit-packed flags. See the `FLAG_*` constants on this type.
    flags: u8,
}

impl OpenOptions {
    const FLAG_READ: u8 = 1 << 0;
    const FLAG_WRITE: u8 = 1 << 1;
    const FLAG_CREATE: u8 = 1 << 2;
    const FLAG_CREATE_NEW: u8 = 1 << 3;
    const FLAG_TRUNCATE: u8 = 1 << 4;
    const FLAG_APPEND: u8 = 1 << 5;

    /// Create new open options with all flags set to false.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn set_flag(mut self, flag: u8, value: bool) -> Self {
        if value {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
        self
    }
}

macro_rules! flag_setters {
    ($($(#[$attr:meta])* $name:ident => $flag:ident),* $(,)?) => {
        impl OpenOptions {
            $(
                $(#[$attr])*
                #[must_use]
                pub fn $name(self, value: bool) -> Self {
                    self.set_flag(Self::$flag, value)
                }
            )*
        }
    };
}

flag_setters! {
    /// Set the read flag.
    read => FLAG_READ,
    /// Set the write flag.
    write => FLAG_WRITE,
    /// Set the create flag.
    create => FLAG_CREATE,
    /// Set the `create_new` flag.
    create_new => FLAG_CREATE_NEW,
    /// Set the truncate flag.
    truncate => FLAG_TRUNCATE,
    /// Set the append flag.
    append => FLAG_APPEND,
}

impl OpenOptions {

    /// Returns true if the file will be opened for reading.
    #[must_use]
    pub fn is_read(&self) -> bool {
        self.flags & Self::FLAG_READ != 0
    }

    /// Returns true if the file will be opened for writing.
    #[must_use]
    pub fn is_write(&self) -> bool {
        self.flags & Self::FLAG_WRITE != 0
    }

    /// Returns true if the file will be created if it does not exist.
    #[must_use]
    pub fn is_create(&self) -> bool {
        self.flags & Self::FLAG_CREATE != 0
    }

    /// Returns true if the file must be created new (failing if it exists).
    #[must_use]
    pub fn is_create_new(&self) -> bool {
        self.flags & Self::FLAG_CREATE_NEW != 0
    }

    /// Returns true if the file will be truncated to zero length on open.
    #[must_use]
    pub fn is_truncate(&self) -> bool {
        self.flags & Self::FLAG_TRUNCATE != 0
    }

    /// Returns true if writes will be appended to the end of the file.
    #[must_use]
    pub fn is_append(&self) -> bool {
        self.flags & Self::FLAG_APPEND != 0
    }

    /// Create options for read-only access.
    #[must_use]
    pub fn read_only() -> Self {
        Self::new().read(true)
    }

    /// Create options for creating and writing a new file (truncating if exists).
    #[must_use]
    pub fn create_write() -> Self {
        Self::new().write(true).create(true).truncate(true)
    }

    /// Create options for creating a new file for writing (fails if exists).
    #[must_use]
    pub fn create_new_write() -> Self {
        Self::new().write(true).create_new(true)
    }
}

/// Provider trait for file storage operations.
///
/// Clone allows sharing providers across multiple components efficiently.
pub trait StorageProvider: Clone + Send + Sync + 'static {
    /// The file type for this provider.
    type File: StorageFile + 'static;

    /// Open a file with the given options.
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
}

/// Trait for file handles that support async read/write/seek operations.
pub trait StorageFile: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + Sync + 'static {
    /// Flush all OS-internal metadata and data to disk.
    fn sync_all(&self) -> impl std::future::Future<Output = io::Result<()>> + Send;

    /// Flush all data to disk (metadata may not be synced).
    fn sync_data(&self) -> impl std::future::Future<Output = io::Result<()>> + Send;

    /// Get the current size of the file in bytes.
    fn size(&self) -> impl std::future::Future<Output = io::Result<u64>> + Send;

    /// Set the length of the file.
    fn set_len(&self, size: u64) -> impl std::future::Future<Output = io::Result<()>> + Send;
}

/// Real Tokio storage implementation.
#[cfg(feature = "tokio-providers")]
#[derive(Debug, Clone, Default)]
pub struct TokioStorageProvider;

#[cfg(feature = "tokio-providers")]
impl TokioStorageProvider {
    /// Create a new Tokio storage provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "tokio-providers")]
impl StorageProvider for TokioStorageProvider {
    type File = TokioStorageFile;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        let file = tokio::fs::OpenOptions::new()
            .read(options.is_read())
            .write(options.is_write())
            .create(options.is_create())
            .create_new(options.is_create_new())
            .truncate(options.is_truncate())
            .append(options.is_append())
            .open(path)
            .await?;
        Ok(TokioStorageFile {
            inner: file.compat(),
        })
    }

    async fn exists(&self, path: &str) -> io::Result<bool> {
        match tokio::fs::metadata(path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, path: &str) -> io::Result<()> {
        tokio::fs::remove_file(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        tokio::fs::rename(from, to).await
    }
}

/// Wrapper for Tokio File to implement our trait.
///
/// Holds the underlying `tokio::fs::File` inside `tokio_util::compat::Compat`
/// so the `futures::io` trait impls come through automatically. The four custom
/// methods on [`StorageFile`] reach the inner [`tokio::fs::File`] via
/// [`Compat::get_ref`] / [`Compat::get_mut`] to call tokio-specific APIs
/// (`sync_all`, `sync_data`, `metadata`, `set_len`) that `Compat` itself
/// does not expose.
#[cfg(feature = "tokio-providers")]
#[derive(Debug)]
pub struct TokioStorageFile {
    inner: Compat<tokio::fs::File>,
}

#[cfg(feature = "tokio-providers")]
impl StorageFile for TokioStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        self.inner.get_ref().sync_all().await
    }

    async fn sync_data(&self) -> io::Result<()> {
        self.inner.get_ref().sync_data().await
    }

    async fn size(&self) -> io::Result<u64> {
        let metadata = self.inner.get_ref().metadata().await?;
        Ok(metadata.len())
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.get_ref().set_len(size).await
    }
}

#[cfg(feature = "tokio-providers")]
impl AsyncRead for TokioStorageFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[cfg(feature = "tokio-providers")]
impl AsyncWrite for TokioStorageFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[cfg(feature = "tokio-providers")]
impl AsyncSeek for TokioStorageFile {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.inner).poll_seek(cx, pos)
    }
}
