//! Production [`StorageProvider`] / [`StorageFile`] over the real filesystem.

use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

use super::{AlignedBuf, DirectIo, IoConstraints, OpenOptions, StorageFile, StorageProvider};

/// Alignment assumed for a direct-I/O file.
///
/// A conservative superset: 4 KiB satisfies both 512-byte and 4 KiB logical
/// block sizes, so a transfer legal here is legal on either device. Querying
/// the device for a smaller value would only *widen* what callers may do.
const DIRECT_IO_ALIGNMENT: usize = 4096;

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
        let (file, direct) = open_file(path, &options).await?;
        // A second descriptor onto the same open file description, used for
        // positioned I/O: `read_at`/`write_at` must not disturb the stream
        // cursor, and the OS positioned calls need a blocking `std::fs::File`.
        let positioned = file.try_clone().await?.into_std().await;
        Ok(TokioStorageFile {
            inner: file.compat(),
            positioned: Arc::new(positioned),
            constraints: if direct {
                IoConstraints::uniform(DIRECT_IO_ALIGNMENT)
            } else {
                IoConstraints::NONE
            },
            direct,
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

    async fn sync_dir(&self, path: &str) -> io::Result<()> {
        sync_dir_impl(path).await
    }
}

/// Fsync a directory so its entries survive a crash.
///
/// On unix this is `open(dir, O_RDONLY)` + `fsync`, the portable idiom.
#[cfg(unix)]
async fn sync_dir_impl(path: &str) -> io::Result<()> {
    let path = path.to_string();
    run_blocking(move || std::fs::File::open(&path)?.sync_all()).await
}

/// Windows has no directory handle to fsync: metadata operations on NTFS are
/// journaled, and the ordering guarantee this call buys on unix comes for
/// free. Succeeds without doing anything.
#[cfg(not(unix))]
async fn sync_dir_impl(_path: &str) -> io::Result<()> {
    Ok(())
}

/// Build the tokio open options for `options`, optionally adding `O_DIRECT`.
fn open_options(options: &OpenOptions, direct: bool) -> tokio::fs::OpenOptions {
    let mut builder = tokio::fs::OpenOptions::new();
    builder
        .read(options.is_read())
        .write(options.is_write())
        .create(options.is_create())
        .create_new(options.is_create_new())
        .truncate(options.is_truncate())
        .append(options.is_append());
    #[cfg(target_os = "linux")]
    if direct {
        builder.custom_flags(libc::O_DIRECT);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = direct;
    builder
}

/// Whether this build can ask the kernel for uncached I/O at all.
const DIRECT_IO_SUPPORTED: bool = cfg!(target_os = "linux");

/// Open `path`, honoring the requested direct-I/O policy.
///
/// Returns the file and whether direct I/O was actually achieved. `Optional`
/// falls back to a buffered open when the filesystem rejects the flag (tmpfs
/// and several network filesystems do); `Required` never falls back.
///
/// A `create_new` open that fell back would fail with `AlreadyExists`, since
/// the rejected attempt may still have created the file — so the fallback
/// retries without the exclusivity that the first attempt already satisfied.
async fn open_file(path: &str, options: &OpenOptions) -> io::Result<(tokio::fs::File, bool)> {
    match options.requested_direct_io() {
        DirectIo::Disabled => Ok((open_options(options, false).open(path).await?, false)),
        DirectIo::Required if !DIRECT_IO_SUPPORTED => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "direct I/O is not supported on this platform",
        )),
        DirectIo::Required => Ok((open_options(options, true).open(path).await?, true)),
        DirectIo::Optional => {
            if DIRECT_IO_SUPPORTED && let Ok(file) = open_options(options, true).open(path).await {
                return Ok((file, true));
            }
            let fallback = options.clone().create_new(false);
            Ok((open_options(&fallback, false).open(path).await?, false))
        }
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
    constraints: IoConstraints,
    direct: bool,
}

/// One positioned read against the OS.
///
/// Runs on the blocking pool through a bounce buffer: the caller's slice
/// cannot be moved into a `'static` blocking closure, so the transfer lands in
/// an owned buffer that is copied back on completion. The bounce buffer
/// carries the file's memory alignment — a direct-I/O transfer through a
/// byte-aligned `Vec` earns `EINVAL` however carefully the caller aligned the
/// slice it handed in.
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

impl TokioStorageFile {
    /// A `len`-byte bounce buffer aligned the way this file's transfers must
    /// be.
    fn bounce(&self, len: usize) -> AlignedBuf {
        AlignedBuf::zeroed(len, self.constraints.memory_alignment())
    }
}

impl StorageFile for TokioStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        self.inner.get_ref().sync_all().await
    }

    async fn sync_data(&self) -> io::Result<()> {
        self.inner.get_ref().sync_data().await
    }

    fn constraints(&self) -> IoConstraints {
        self.constraints
    }

    fn is_direct_io(&self) -> bool {
        self.direct
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        let file = Arc::clone(&self.positioned);
        let mut bounce = self.bounce(buf.len());
        let (read, bounce) = run_blocking(move || {
            let read = read_at_blocking(&file, offset, bounce.as_mut_slice())?;
            Ok((read, bounce))
        })
        .await?;
        buf[..read].copy_from_slice(&bounce.as_slice()[..read]);
        Ok(read)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        let file = Arc::clone(&self.positioned);
        let mut bounce = self.bounce(buf.len());
        bounce.as_mut_slice().copy_from_slice(buf);
        run_blocking(move || write_at_blocking(&file, offset, bounce.as_slice())).await
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
