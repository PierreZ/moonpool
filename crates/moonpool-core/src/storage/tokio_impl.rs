//! Production [`StorageProvider`] / [`StorageFile`] over the real filesystem.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tracing::instrument;

use super::native::{self, NativeFs};
use super::{AlignedBuf, IoConstraints, OpenOptions, StorageFile, StorageProvider};

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
        Ok(TokioStorageFile {
            file: Arc::new(opened.file.into_std().await),
            constraints: opened.constraints,
            direct: opened.direct,
            dsync_refused: Arc::new(AtomicBool::new(false)),
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
        .truncate(options.is_truncate());
    #[cfg(target_os = "linux")]
    if direct {
        builder.custom_flags(libc::O_DIRECT);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = direct;
    builder
}

/// One open file of [`TokioStorageProvider`].
///
/// Every transfer is positioned and runs on the blocking pool against the
/// descriptor. No transfer uses the descriptor's cursor.
#[derive(Debug)]
pub struct TokioStorageFile {
    file: Arc<std::fs::File>,
    constraints: IoConstraints,
    direct: bool,
    /// Set once the filesystem of this file refused a durable write with
    /// `RWF_DSYNC` (`EOPNOTSUPP`): later durable writes on this file use a
    /// write and a sync. Other files keep their own flag, because they can
    /// be on another filesystem.
    dsync_refused: Arc<AtomicBool>,
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
/// `O_DSYNC`, it covers this write's range only. If the kernel refuses the
/// call, the write falls back to the portable pair (see [`choose_durable`]).
#[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
fn write_at_durable_blocking(
    file: &std::fs::File,
    dsync_refused: &AtomicBool,
    offset: u64,
    buf: &[u8],
) -> io::Result<usize> {
    use std::os::fd::AsRawFd as _;

    /// Set once the kernel has no `pwritev2` (`ENOSYS`): that is true of
    /// every file, so every later call of the process goes straight to the
    /// fallback.
    static PWRITEV2_MISSING: AtomicBool = AtomicBool::new(false);

    let position = libc::off_t::try_from(offset)
        .map_err(|_| super::invalid_input(format!("offset {offset} overflows off_t")))?;
    choose_durable(
        &PWRITEV2_MISSING,
        dsync_refused,
        || {
            let iov = libc::iovec {
                iov_base: buf.as_ptr().cast_mut().cast::<libc::c_void>(),
                iov_len: buf.len(),
            };
            // SAFETY: the descriptor is open for the duration of the call,
            // `iov` describes exactly `buf`, which outlives the call, and the
            // kernel only reads through it (a write never stores into the
            // source buffer).
            let written = unsafe {
                libc::pwritev2(
                    file.as_raw_fd(),
                    &raw const iov,
                    1,
                    position,
                    libc::RWF_DSYNC,
                )
            };
            usize::try_from(written).map_err(|_| io::Error::last_os_error())
        },
        || write_then_sync_blocking(file, offset, buf),
    )
}

/// Run `dsync` (one write with its own data-sync), or `fallback` (a write,
/// then a sync) where `dsync` is known to fail.
///
/// - `ENOSYS`: the kernel has no `pwritev2`. This is true of every file, so
///   it sets `process_missing`, and all later calls of the process use
///   `fallback`.
/// - `EOPNOTSUPP`: the filesystem of this file refuses `RWF_DSYNC`. Another
///   filesystem can accept it, so it sets `file_refused` only, and only the
///   later calls on this file use `fallback`.
///
/// Both errors write no bytes, so the fallback writes them all.
#[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
fn choose_durable(
    process_missing: &AtomicBool,
    file_refused: &AtomicBool,
    dsync: impl FnOnce() -> io::Result<usize>,
    fallback: impl FnOnce() -> io::Result<usize>,
) -> io::Result<usize> {
    use std::sync::atomic::Ordering;

    if process_missing.load(Ordering::Relaxed) || file_refused.load(Ordering::Relaxed) {
        return fallback();
    }
    match dsync() {
        Ok(written) => Ok(written),
        Err(error) => match error.raw_os_error() {
            Some(libc::ENOSYS) => {
                process_missing.store(true, Ordering::Relaxed);
                fallback()
            }
            Some(libc::EOPNOTSUPP) => {
                file_refused.store(true, Ordering::Relaxed);
                fallback()
            }
            _ => Err(error),
        },
    }
}

#[cfg(not(all(target_os = "linux", any(target_env = "gnu", target_env = "musl"))))]
fn write_at_durable_blocking(
    file: &std::fs::File,
    _dsync_refused: &AtomicBool,
    offset: u64,
    buf: &[u8],
) -> io::Result<usize> {
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
        let file = Arc::clone(&self.file);
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
        let file = Arc::clone(&self.file);
        let dsync_refused = Arc::clone(&self.dsync_refused);
        run_blocking(move || {
            let written = match kind {
                WriteKind::Visible => write_at_blocking(&file, offset, buf.as_slice())?,
                WriteKind::Durable => {
                    write_at_durable_blocking(&file, &dsync_refused, offset, buf.as_slice())?
                }
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
        let file = Arc::clone(&self.file);
        run_blocking(move || file.sync_all()).await
    }

    async fn sync_data(&self) -> io::Result<()> {
        let file = Arc::clone(&self.file);
        run_blocking(move || file.sync_data()).await
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
        let file = Arc::clone(&self.file);
        run_blocking(move || Ok(file.metadata()?.len())).await
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        let file = Arc::clone(&self.file);
        run_blocking(move || file.set_len(size)).await
    }

    async fn allocate(&self, len: u64) -> io::Result<()> {
        let file = Arc::clone(&self.file);
        run_blocking(move || allocate_blocking(&file, len)).await
    }

    async fn drop_cache(&self) -> io::Result<()> {
        if self.direct {
            return Ok(());
        }
        let file = Arc::clone(&self.file);
        run_blocking(move || native::drop_cache(&*file)).await
    }
}

#[cfg(all(
    test,
    target_os = "linux",
    any(target_env = "gnu", target_env = "musl")
))]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::choose_durable;

    /// One durable write through [`choose_durable`]: `dsync` returns
    /// `dsync_result`, the fallback writes 4 bytes. Returns the result and
    /// whether `dsync` was tried.
    fn durable_write(
        process_missing: &AtomicBool,
        file_refused: &AtomicBool,
        dsync_result: fn() -> io::Result<usize>,
    ) -> (io::Result<usize>, bool) {
        let tried = Cell::new(false);
        let result = choose_durable(
            process_missing,
            file_refused,
            || {
                tried.set(true);
                dsync_result()
            },
            || Ok(4),
        );
        (result, tried.get())
    }

    /// `EOPNOTSUPP` comes from the filesystem of one file. It moves that
    /// file to the fallback, and another file still tries `RWF_DSYNC`.
    #[test]
    fn one_file_refusing_dsync_does_not_change_another_file() {
        let process_missing = AtomicBool::new(false);
        let refusing = AtomicBool::new(false);
        let other = AtomicBool::new(false);
        let refuse = || Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));

        let (result, tried) = durable_write(&process_missing, &refusing, refuse);
        assert_eq!(result.ok(), Some(4), "the fallback writes the bytes");
        assert!(tried);
        assert!(refusing.load(Ordering::Relaxed));
        assert!(!process_missing.load(Ordering::Relaxed));

        let (result, tried) = durable_write(&process_missing, &refusing, || Ok(8));
        assert_eq!(result.ok(), Some(4));
        assert!(!tried, "the refusing file goes straight to the fallback");

        let (result, tried) = durable_write(&process_missing, &other, || Ok(8));
        assert_eq!(result.ok(), Some(8));
        assert!(tried, "another file still tries RWF_DSYNC");
        assert!(!other.load(Ordering::Relaxed));
    }

    /// `ENOSYS` means the kernel has no `pwritev2`: true of every file, so
    /// every file uses the fallback after it.
    #[test]
    fn missing_pwritev2_moves_every_file_to_the_fallback() {
        let process_missing = AtomicBool::new(false);
        let first = AtomicBool::new(false);
        let other = AtomicBool::new(false);
        let missing = || Err(io::Error::from_raw_os_error(libc::ENOSYS));

        let (result, _) = durable_write(&process_missing, &first, missing);
        assert_eq!(result.ok(), Some(4));
        assert!(process_missing.load(Ordering::Relaxed));

        let (result, tried) = durable_write(&process_missing, &other, || Ok(8));
        assert_eq!(result.ok(), Some(4));
        assert!(!tried);
    }

    /// Any other error is the write's own failure: no fallback, no flag.
    #[test]
    fn other_errors_are_returned() {
        let process_missing = AtomicBool::new(false);
        let file = AtomicBool::new(false);
        let (result, _) = durable_write(&process_missing, &file, || {
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
        assert_eq!(
            result.err().and_then(|error| error.raw_os_error()),
            Some(libc::EIO)
        );
        assert!(!file.load(Ordering::Relaxed));
        assert!(!process_missing.load(Ordering::Relaxed));
    }
}
