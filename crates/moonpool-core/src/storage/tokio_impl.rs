//! Production [`StorageProvider`] / [`StorageFile`] over the real filesystem.

use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};
use tracing::instrument;

use super::native::{self, NativeFs, OpenedFile};
use super::{
    AlignedBuf, IoConstraints, OpenOptions, StorageFile, StorageProvider, stream_io_unsupported,
};

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

    #[instrument(skip(self))]
    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        let opened = native::open_file(&TokioFs, path, &options).await?;
        let positioned = positioned_descriptor(&opened, path, &options).await?;
        Ok(TokioStorageFile {
            inner: opened.file.compat(),
            positioned: Arc::new(positioned),
            constraints: opened.constraints,
            direct: opened.direct,
        })
    }

    async fn exists(&self, path: &str) -> io::Result<bool> {
        tokio::fs::try_exists(path).await
    }

    #[instrument(skip(self))]
    async fn delete(&self, path: &str) -> io::Result<()> {
        tokio::fs::remove_file(path).await
    }

    #[instrument(skip(self))]
    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        tokio::fs::rename(from, to).await
    }

    #[instrument(skip(self))]
    async fn create_dir_all(&self, path: &str) -> io::Result<()> {
        tokio::fs::create_dir_all(path).await
    }

    #[instrument(skip(self))]
    async fn list_dir(&self, path: &str) -> io::Result<Vec<String>> {
        let mut entries = tokio::fs::read_dir(path).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().into_string().map_err(|name| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("directory entry {} is not valid UTF-8", name.display()),
                )
            })?;
            names.push(name);
        }
        names.sort_unstable();
        Ok(names)
    }

    #[instrument(skip(self))]
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
    run_blocking(move || {
        let directory = std::fs::File::open(&path)?;
        if !directory.metadata()?.is_dir() {
            return Err(not_a_directory());
        }
        directory.sync_all()
    })
    .await
}

/// Windows has no directory handle to fsync: metadata operations on NTFS are
/// journaled, and the ordering guarantee this call buys on unix comes for
/// free. Validates the directory, then succeeds without flushing it.
#[cfg(not(unix))]
async fn sync_dir_impl(path: &str) -> io::Result<()> {
    if !tokio::fs::metadata(path).await?.is_dir() {
        return Err(not_a_directory());
    }
    Ok(())
}

fn not_a_directory() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotADirectory,
        "sync_dir requires a directory",
    )
}

/// Tokio's half of the shared open policy (see [`native`]).
#[derive(Debug, Clone, Copy)]
struct TokioFs;

impl NativeFs for TokioFs {
    type File = tokio::fs::File;

    async fn open(
        &self,
        path: &std::path::Path,
        options: &OpenOptions,
        direct: bool,
    ) -> io::Result<tokio::fs::File> {
        open_options(options, direct).open(path).await
    }

    async fn truncate(file: &tokio::fs::File) -> io::Result<()> {
        file.set_len(0).await
    }
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

/// The descriptor positioned I/O runs on.
///
/// `read_at`/`write_at` must not disturb the stream cursor, and the OS
/// positioned calls need a blocking `std::fs::File`, so they get a descriptor
/// of their own.
///
/// # Why an append file cannot simply clone
///
/// `try_clone` is `dup`, which shares the *open file description* and
/// therefore its status flags — `O_APPEND` among them. On Linux a `pwrite` to
/// an `O_APPEND` descriptor ignores the supplied offset and appends, so a
/// cloned descriptor would make `write_at` do the one thing its contract says
/// it never does. Clearing the flag is not an option either: `F_SETFL` acts on
/// the shared description, so it would disable append for the stream writes
/// that asked for it.
///
/// An append file therefore gets a genuinely separate open, without
/// `O_APPEND`, of unambiguously the same file (see [`native::reopen_same_file`]).
/// Without append the clone carries identical flags and is exactly
/// equivalent, so it is kept — one fewer open, and no dependency on the
/// descriptor path.
async fn positioned_descriptor(
    opened: &OpenedFile<tokio::fs::File>,
    path: &str,
    options: &OpenOptions,
) -> io::Result<std::fs::File> {
    if !options.is_append() {
        return Ok(opened.file.try_clone().await?.into_std().await);
    }
    // Append implies write access; the read side is carried over as asked for,
    // so a file opened without read still refuses `read_at`.
    let positioned = OpenOptions::new().read(options.is_read()).write(true);
    let reopened =
        native::reopen_same_file(&TokioFs, &opened.file, path, &positioned, opened.direct).await?;
    Ok(reopened.into_std().await)
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
/// Runs on the blocking pool, into an owned [`AlignedBuf`] moved into the
/// blocking closure and back. The owned methods
/// ([`read_at_aligned`](StorageFile::read_at_aligned) and its siblings) pass
/// the caller's buffer itself: no copy. The borrowed ones cannot — a slice
/// cannot move into a `'static` closure — so they go through a bounce
/// buffer copied in before a write and out after a read. The bounce buffer
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

/// One durable positioned write against the OS: the bytes it reports
/// written are durable when it returns.
///
/// On Linux this is `pwritev2(RWF_DSYNC)`: the write carries its own
/// data-sync, one syscall where `pwrite` plus `fdatasync` are two, and on a
/// direct-I/O file whose blocks are already allocated the kernel can turn it
/// into a single FUA write instead of a write and a cache flush. Like
/// `O_DSYNC`, it covers this write's range only. A kernel without
/// `pwritev2` (`ENOSYS`) or without `RWF_DSYNC` (`EOPNOTSUPP`) falls back to
/// the portable pair, once for the process.
#[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
fn write_at_durable_blocking(file: &std::fs::File, offset: u64, buf: &[u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd as _;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set once the kernel refused `RWF_DSYNC`: every later call goes
    /// straight to the fallback instead of failing the same syscall again.
    static DSYNC_REFUSED: AtomicBool = AtomicBool::new(false);

    if DSYNC_REFUSED.load(Ordering::Relaxed) {
        return write_then_sync_blocking(file, offset, buf);
    }
    let position = libc::off_t::try_from(offset)
        .map_err(|_| super::invalid_input(format!("offset {offset} overflows off_t")))?;
    let iov = libc::iovec {
        iov_base: buf.as_ptr().cast_mut().cast::<libc::c_void>(),
        iov_len: buf.len(),
    };
    // SAFETY: the descriptor is open for the duration of the call, `iov`
    // describes exactly `buf`, which outlives the call, and the kernel only
    // reads through it (a write never stores into the source buffer).
    let written = unsafe {
        libc::pwritev2(
            file.as_raw_fd(),
            &raw const iov,
            1,
            position,
            libc::RWF_DSYNC,
        )
    };
    if let Ok(written) = usize::try_from(written) {
        return Ok(written);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOSYS | libc::EOPNOTSUPP) => {
            DSYNC_REFUSED.store(true, Ordering::Relaxed);
            write_then_sync_blocking(file, offset, buf)
        }
        _ => Err(error),
    }
}

#[cfg(not(all(target_os = "linux", any(target_env = "gnu", target_env = "musl"))))]
fn write_at_durable_blocking(file: &std::fs::File, offset: u64, buf: &[u8]) -> io::Result<usize> {
    write_then_sync_blocking(file, offset, buf)
}

/// The portable durable write: a positioned write, then a data sync, both on
/// the one blocking task. The sync covers more than the write's range, which
/// the contract allows.
fn write_then_sync_blocking(file: &std::fs::File, offset: u64, buf: &[u8]) -> io::Result<usize> {
    let written = write_at_blocking(file, offset, buf)?;
    file.sync_data()?;
    Ok(written)
}

/// [`StorageFile::allocate`] on the blocking pool: `fallocate`, else grow.
fn allocate_blocking(file: &std::fs::File, len: u64) -> io::Result<()> {
    if !native::fallocate(file, len)? && file.metadata()?.len() < len {
        file.set_len(len)?;
    }
    Ok(())
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

/// Which positioned write an owned transfer performs.
#[derive(Debug, Clone, Copy)]
enum WriteKind {
    /// [`StorageFile::write_at`]: visible on return.
    Visible,
    /// [`StorageFile::write_at_durable`]: durable on return.
    Durable,
}

impl TokioStorageFile {
    /// A bounce buffer holding a copy of `buf`, aligned the way this file's
    /// transfers must be: what the borrowed write methods pay that the owned
    /// ones do not.
    fn bounce_from(&self, buf: &[u8]) -> AlignedBuf {
        let mut bounce = AlignedBuf::zeroed(buf.len(), self.constraints.memory_alignment());
        bounce.as_mut_slice().copy_from_slice(buf);
        bounce
    }

    /// The owned read: `buf` moves into the blocking task and back.
    async fn read_owned(&self, offset: u64, buf: AlignedBuf) -> io::Result<(usize, AlignedBuf)> {
        if buf.is_empty() {
            return Ok((0, buf));
        }
        self.constraints.check(offset, buf.as_slice())?;
        let file = Arc::clone(&self.positioned);
        run_blocking(move || {
            let mut buf = buf;
            let read = read_at_blocking(&file, offset, buf.as_mut_slice())?;
            Ok((read, buf))
        })
        .await
    }

    /// The owned write, visible or durable: `buf` moves into the blocking
    /// task and back.
    async fn write_owned(
        &self,
        offset: u64,
        buf: AlignedBuf,
        kind: WriteKind,
    ) -> io::Result<(usize, AlignedBuf)> {
        if buf.is_empty() {
            return Ok((0, buf));
        }
        self.constraints.check(offset, buf.as_slice())?;
        let file = Arc::clone(&self.positioned);
        run_blocking(move || {
            let written = match kind {
                WriteKind::Visible => write_at_blocking(&file, offset, buf.as_slice())?,
                WriteKind::Durable => write_at_durable_blocking(&file, offset, buf.as_slice())?,
            };
            Ok((written, buf))
        })
        .await
    }

    /// The borrowed write: checked against the caller's own slice (the
    /// contract is the caller's, whatever this backend does next), then
    /// copied into a bounce buffer for the owned write.
    async fn write_borrowed(&self, offset: u64, buf: &[u8], kind: WriteKind) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        let (written, _) = self
            .write_owned(offset, self.bounce_from(buf), kind)
            .await?;
        Ok(written)
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
        let bounce = AlignedBuf::zeroed(buf.len(), self.constraints.memory_alignment());
        let (read, bounce) = self.read_owned(offset, bounce).await?;
        buf[..read].copy_from_slice(&bounce.as_slice()[..read]);
        Ok(read)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.write_borrowed(offset, buf, WriteKind::Visible).await
    }

    async fn write_at_durable(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.write_borrowed(offset, buf, WriteKind::Durable).await
    }

    async fn read_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> io::Result<(usize, AlignedBuf)> {
        self.read_owned(offset, buf).await
    }

    async fn write_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> io::Result<(usize, AlignedBuf)> {
        self.write_owned(offset, buf, WriteKind::Visible).await
    }

    async fn write_at_durable_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> io::Result<(usize, AlignedBuf)> {
        self.write_owned(offset, buf, WriteKind::Durable).await
    }

    async fn size(&self) -> io::Result<u64> {
        let metadata = self.inner.get_ref().metadata().await?;
        Ok(metadata.len())
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.get_ref().set_len(size).await
    }

    async fn allocate(&self, len: u64) -> io::Result<()> {
        let file = Arc::clone(&self.positioned);
        run_blocking(move || allocate_blocking(&file, len)).await
    }
}

impl AsyncRead for TokioStorageFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // Refused here rather than left to the kernel's EINVAL, so the
        // contract is the same one the simulation enforces.
        if !self.constraints.is_unconstrained() {
            return Poll::Ready(Err(stream_io_unsupported()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TokioStorageFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.constraints.is_unconstrained() {
            return Poll::Ready(Err(stream_io_unsupported()));
        }
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
