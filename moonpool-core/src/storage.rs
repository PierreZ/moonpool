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
/// should be opened, similar to [`std::fs::OpenOptions`].
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Open file for reading.
    pub read: bool,
    /// Open file for writing.
    pub write: bool,
    /// Create the file if it doesn't exist.
    pub create: bool,
    /// Create a new file, failing if it already exists.
    pub create_new: bool,
    /// Truncate the file to zero length.
    pub truncate: bool,
    /// Append to the end of the file.
    pub append: bool,
}

impl OpenOptions {
    /// Create new open options with all flags set to false.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the read flag.
    pub fn read(mut self, read: bool) -> Self {
        self.read = read;
        self
    }

    /// Set the write flag.
    pub fn write(mut self, write: bool) -> Self {
        self.write = write;
        self
    }

    /// Set the create flag.
    pub fn create(mut self, create: bool) -> Self {
        self.create = create;
        self
    }

    /// Set the create_new flag.
    pub fn create_new(mut self, create_new: bool) -> Self {
        self.create_new = create_new;
        self
    }

    /// Set the truncate flag.
    pub fn truncate(mut self, truncate: bool) -> Self {
        self.truncate = truncate;
        self
    }

    /// Set the append flag.
    pub fn append(mut self, append: bool) -> Self {
        self.append = append;
        self
    }

    /// Create options for read-only access.
    pub fn read_only() -> Self {
        Self::new().read(true)
    }

    /// Create options for creating and writing a new file (truncating if exists).
    pub fn create_write() -> Self {
        Self::new().write(true).create(true).truncate(true)
    }

    /// Create options for creating a new file for writing (fails if exists).
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
#[derive(Debug, Clone)]
pub struct TokioStorageProvider;

#[cfg(feature = "tokio-providers")]
impl TokioStorageProvider {
    /// Create a new Tokio storage provider.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "tokio-providers")]
impl Default for TokioStorageProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "tokio-providers")]
impl StorageProvider for TokioStorageProvider {
    type File = TokioStorageFile;

    fn open(
        &self,
        path: &str,
        options: OpenOptions,
    ) -> impl std::future::Future<Output = io::Result<Self::File>> + Send {
        let path = path.to_string();
        async move {
            let file = tokio::fs::OpenOptions::new()
                .read(options.read)
                .write(options.write)
                .create(options.create)
                .create_new(options.create_new)
                .truncate(options.truncate)
                .append(options.append)
                .open(&path)
                .await?;
            Ok(TokioStorageFile {
                inner: file.compat(),
            })
        }
    }

    fn exists(&self, path: &str) -> impl std::future::Future<Output = io::Result<bool>> + Send {
        let path = path.to_string();
        async move {
            match tokio::fs::metadata(&path).await {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            }
        }
    }

    fn delete(&self, path: &str) -> impl std::future::Future<Output = io::Result<()>> + Send {
        let path = path.to_string();
        async move { tokio::fs::remove_file(&path).await }
    }

    fn rename(
        &self,
        from: &str,
        to: &str,
    ) -> impl std::future::Future<Output = io::Result<()>> + Send {
        let from = from.to_string();
        let to = to.to_string();
        async move { tokio::fs::rename(&from, &to).await }
    }
}

/// Wrapper for Tokio File to implement our trait.
///
/// Holds the underlying `tokio::fs::File` inside `tokio_util::compat::Compat`
/// so the futures::io trait impls come through automatically. The four custom
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
    fn sync_all(&self) -> impl std::future::Future<Output = io::Result<()>> + Send {
        async move { self.inner.get_ref().sync_all().await }
    }

    fn sync_data(&self) -> impl std::future::Future<Output = io::Result<()>> + Send {
        async move { self.inner.get_ref().sync_data().await }
    }

    fn size(&self) -> impl std::future::Future<Output = io::Result<u64>> + Send {
        async move {
            let metadata = self.inner.get_ref().metadata().await?;
            Ok(metadata.len())
        }
    }

    fn set_len(&self, size: u64) -> impl std::future::Future<Output = io::Result<()>> + Send {
        async move { self.inner.get_ref().set_len(size).await }
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
