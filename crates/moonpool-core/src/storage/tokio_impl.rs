//! Production [`StorageProvider`] / [`StorageFile`] over the real filesystem.

use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

use super::{OpenOptions, StorageFile, StorageProvider};

/// Real Tokio storage implementation.
#[derive(Debug, Clone, Default)]
pub struct TokioStorageProvider;

impl TokioStorageProvider {
    /// Create a new Tokio storage provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

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
        // A second descriptor onto the same open file description, used for
        // positioned I/O: `read_at`/`write_at` must not disturb the stream
        // cursor, and the OS positioned calls need a blocking `std::fs::File`.
        let positioned = file.try_clone().await?.into_std().await;
        Ok(TokioStorageFile {
            inner: file.compat(),
            positioned: Arc::new(positioned),
        })
    }

    async fn exists(&self, path: &str) -> io::Result<bool> {
        tokio::fs::try_exists(path).await
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
#[derive(Debug)]
pub struct TokioStorageFile {
    inner: Compat<tokio::fs::File>,
    /// Duplicated descriptor for positioned I/O. It shares the open file
    /// description (and therefore the file's contents and open flags) with
    /// `inner`, but positioned calls use neither descriptor's cursor.
    positioned: Arc<std::fs::File>,
}

/// One positioned read against the OS.
///
/// Runs on the blocking pool through a bounce buffer: the caller's slice
/// cannot be moved into a `'static` blocking closure, so the transfer lands in
/// an owned buffer that is copied back on completion.
#[cfg(unix)]
fn read_at_blocking(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at_blocking(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_at_blocking(_file: &std::fs::File, _offset: u64, _buf: &mut [u8]) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positioned reads need an OS pread equivalent",
    ))
}

#[cfg(unix)]
fn write_at_blocking(file: &std::fs::File, offset: u64, buf: &[u8]) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

#[cfg(windows)]
fn write_at_blocking(file: &std::fs::File, offset: u64, buf: &[u8]) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

#[cfg(not(any(unix, windows)))]
fn write_at_blocking(_file: &std::fs::File, _offset: u64, _buf: &[u8]) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positioned writes need an OS pwrite equivalent",
    ))
}

async fn run_blocking<T, F>(work: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| io::Error::other(format!("blocking file I/O task failed: {error}")))?
}

impl StorageFile for TokioStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        self.inner.get_ref().sync_all().await
    }

    async fn sync_data(&self) -> io::Result<()> {
        self.inner.get_ref().sync_data().await
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let file = Arc::clone(&self.positioned);
        let mut bounce = vec![0u8; buf.len()];
        let (read, bounce) = run_blocking(move || {
            let read = read_at_blocking(&file, offset, &mut bounce)?;
            Ok((read, bounce))
        })
        .await?;
        buf[..read].copy_from_slice(&bounce[..read]);
        Ok(read)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let file = Arc::clone(&self.positioned);
        let bounce = buf.to_vec();
        run_blocking(move || write_at_blocking(&file, offset, &bounce)).await
    }

    async fn size(&self) -> io::Result<u64> {
        let metadata = self.inner.get_ref().metadata().await?;
        Ok(metadata.len())
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.get_ref().set_len(size).await
    }
}

impl AsyncRead for TokioStorageFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
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

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

impl AsyncSeek for TokioStorageFile {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.inner).poll_seek(cx, pos)
    }
}
