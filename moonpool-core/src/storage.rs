//! Storage provider abstraction for simulation and real file I/O.
//!
//! This module provides trait-based file storage that allows seamless swapping
//! between real Tokio file I/O and simulated storage for testing.

use async_trait::async_trait;
use std::io;
use std::io::SeekFrom;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

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

    /// Create options for read and write access.
    pub fn read_write() -> Self {
        Self::new().read(true).write(true)
    }

    /// Create options for creating a new file for writing (fails if exists).
    pub fn create_new_write() -> Self {
        Self::new().write(true).create_new(true)
    }
}

/// Provider trait for file storage operations.
///
/// Single-core design - no Send bounds needed.
/// Clone allows sharing providers across multiple components efficiently.
#[async_trait(?Send)]
pub trait StorageProvider: Clone {
    /// The file type for this provider.
    type File: StorageFile + 'static;

    /// Open a file with the given options.
    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File>;

    /// Check if a file exists at the given path.
    async fn exists(&self, path: &str) -> io::Result<bool>;

    /// Delete a file at the given path.
    async fn delete(&self, path: &str) -> io::Result<()>;

    /// Rename a file from one path to another.
    async fn rename(&self, from: &str, to: &str) -> io::Result<()>;
}

/// Trait for file handles that support async read/write/seek operations.
#[async_trait(?Send)]
pub trait StorageFile: AsyncRead + AsyncWrite + AsyncSeek + Unpin {
    /// Flush all OS-internal metadata and data to disk.
    async fn sync_all(&self) -> io::Result<()>;

    /// Flush all data to disk (metadata may not be synced).
    async fn sync_data(&self) -> io::Result<()>;

    /// Get the current size of the file in bytes.
    async fn size(&self) -> io::Result<u64>;

    /// Set the length of the file.
    async fn set_len(&self, size: u64) -> io::Result<()>;
}

/// Real Tokio storage implementation.
#[derive(Debug, Clone)]
pub struct TokioStorageProvider;

impl TokioStorageProvider {
    /// Create a new Tokio storage provider.
    pub fn new() -> Self {
        Self
    }
}

impl Default for TokioStorageProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl StorageProvider for TokioStorageProvider {
    type File = TokioStorageFile;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        let file = tokio::fs::OpenOptions::new()
            .read(options.read)
            .write(options.write)
            .create(options.create)
            .create_new(options.create_new)
            .truncate(options.truncate)
            .append(options.append)
            .open(path)
            .await?;
        Ok(TokioStorageFile { inner: file })
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
#[derive(Debug)]
pub struct TokioStorageFile {
    inner: tokio::fs::File,
}

#[async_trait(?Send)]
impl StorageFile for TokioStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all().await
    }

    async fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data().await
    }

    async fn size(&self) -> io::Result<u64> {
        let metadata = self.inner.metadata().await?;
        Ok(metadata.len())
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.set_len(size).await
    }
}

impl AsyncRead for TokioStorageFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

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

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl AsyncSeek for TokioStorageFile {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        Pin::new(&mut self.inner).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.inner).poll_complete(cx)
    }
}
