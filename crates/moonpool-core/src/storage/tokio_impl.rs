//! Production [`StorageProvider`] / [`StorageFile`] over the real filesystem.

use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

use super::{
    AlignedBuf, DirectIo, IoConstraints, OpenOptions, StorageFile, StorageProvider,
    stream_io_unsupported,
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

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        let opened = open_file(path, &options).await?;
        // A second descriptor onto the same open file description, used for
        // positioned I/O: `read_at`/`write_at` must not disturb the stream
        // cursor, and the OS positioned calls need a blocking `std::fs::File`.
        let positioned = opened.file.try_clone().await?.into_std().await;
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

/// Whether an open failure means *direct I/O is unavailable here*, as opposed
/// to the open failing on its own merits.
///
/// Linux rejects `O_DIRECT` on a filesystem that cannot provide it with
/// `EINVAL`; some stacked filesystems answer `EOPNOTSUPP`. Every other error —
/// `EACCES`, `EEXIST`, `ENOENT`, `EMFILE`, `EROFS`, … — is a real failure of
/// the open and must reach the caller unchanged, because falling back to a
/// buffered open would turn it into a success with different semantics.
fn is_direct_io_unsupported(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        let code = error.raw_os_error();
        code == Some(libc::EINVAL) || code == Some(libc::EOPNOTSUPP)
    }
    #[cfg(not(unix))]
    {
        error.kind() == io::ErrorKind::Unsupported
    }
}

/// A file the provider has opened, with the I/O constraints it came with.
struct OpenedFile {
    file: tokio::fs::File,
    /// What this open requires of callers: [`IoConstraints::NONE`] for a
    /// buffered file, the device's discovered requirement for a direct one.
    constraints: IoConstraints,
    /// Whether the open actually got uncached I/O.
    direct: bool,
}

impl OpenedFile {
    fn buffered(file: tokio::fs::File) -> Self {
        Self {
            file,
            constraints: IoConstraints::NONE,
            direct: false,
        }
    }

    fn direct(file: tokio::fs::File, constraints: IoConstraints) -> Self {
        Self {
            file,
            constraints,
            direct: true,
        }
    }
}

/// What the kernel will say about a file's direct-I/O alignment.
enum ReportedAlignment {
    /// The kernel described this file's actual requirement.
    Known(IoConstraints),
    /// The kernel says this file cannot do direct I/O at all.
    Refused,
    /// The kernel would not say — an older kernel, or a libc without the
    /// query. The caller falls back to a bound.
    Unknown,
}

/// `STATX_DIOALIGN`, which `libc` does not export.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const STATX_DIOALIGN: libc::c_uint = 0x0000_2000;

/// Ask the kernel what alignment direct I/O on this file actually requires.
///
/// `statx(STATX_DIOALIGN)` (Linux 5.19+) reports the device's real
/// requirement: `stx_dio_mem_align` for the caller's buffer address, and
/// `stx_dio_offset_align` for both file offsets and transfer lengths. Zeroes
/// mean the file cannot do direct I/O at all.
///
/// This is a metadata query on an already-open descriptor, not I/O, so it does
/// not go to the blocking pool.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn reported_alignment(file: &tokio::fs::File) -> ReportedAlignment {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `statx` fills a caller-allocated `struct statx`; the value is
    // zeroed first so every field is initialized whatever the kernel writes.
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open for the duration of the call, the path is
    // an empty C string paired with `AT_EMPTY_PATH` (the documented way to
    // stat a descriptor), and `stx` is a valid, correctly sized output buffer.
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            STATX_DIOALIGN,
            &raw mut stx,
        )
    };
    if result != 0 || stx.stx_mask & STATX_DIOALIGN == 0 {
        return ReportedAlignment::Unknown;
    }
    if stx.stx_dio_mem_align == 0 || stx.stx_dio_offset_align == 0 {
        return ReportedAlignment::Refused;
    }
    let Ok(memory) = usize::try_from(stx.stx_dio_mem_align) else {
        return ReportedAlignment::Unknown;
    };
    let Ok(length) = usize::try_from(stx.stx_dio_offset_align) else {
        return ReportedAlignment::Unknown;
    };
    let offset = u64::from(stx.stx_dio_offset_align);
    if !memory.is_power_of_two() || !length.is_power_of_two() {
        // Nothing in the contract can be built on a non-power-of-two
        // alignment; treat it as if the kernel had said nothing.
        return ReportedAlignment::Unknown;
    }
    ReportedAlignment::Known(IoConstraints::new(offset, length, memory))
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn reported_alignment(_file: &tokio::fs::File) -> ReportedAlignment {
    ReportedAlignment::Unknown
}

/// An alignment guaranteed to satisfy this system's direct I/O, for kernels
/// that will not report the real one.
///
/// The page size is that bound on Linux. Direct-I/O alignment derives from the
/// underlying device's logical block size, and the block layer refuses a
/// logical block size larger than a page; the kernels that lifted that
/// restriction are also the kernels that report `STATX_DIOALIGN`, which is
/// preferred whenever it is available. So the page size is never too small,
/// and being larger than the device needs only narrows what callers may do —
/// which is the direction [`IoConstraints`] is allowed to err in.
#[cfg(unix)]
fn bounded_alignment() -> Option<IoConstraints> {
    // SAFETY: `sysconf` reads a system parameter and has no preconditions.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = usize::try_from(page_size).ok()?;
    if page_size == 0 || !page_size.is_power_of_two() {
        return None;
    }
    Some(IoConstraints::uniform(page_size))
}

#[cfg(not(unix))]
fn bounded_alignment() -> Option<IoConstraints> {
    None
}

/// The constraints a direct-I/O open must advertise, or `None` if this file
/// cannot do direct I/O or nothing can bound what it would require.
///
/// Advertising constraints that are too weak would hand the caller an aligned
/// buffer the device then rejects, so "we do not know" is answered by not
/// offering direct I/O rather than by guessing.
fn direct_io_constraints(file: &tokio::fs::File) -> Option<IoConstraints> {
    match reported_alignment(file) {
        ReportedAlignment::Known(constraints) => Some(constraints),
        ReportedAlignment::Refused => None,
        ReportedAlignment::Unknown => bounded_alignment(),
    }
}

/// The path that reopens *this exact file*, rather than whatever the original
/// path happens to name by the time the upgrade runs.
///
/// `/proc/self/fd/N` is a magic symlink to the file a descriptor holds, so
/// reopening through it cannot pick up a different file. Reopening by name
/// could: a concurrent rename between the two opens would hand back somebody
/// else's file, and `create_new`'s guarantee — that the caller holds the file
/// it exclusively created — would last only until someone renamed over the
/// name.
#[cfg(target_os = "linux")]
fn descriptor_path(file: &tokio::fs::File, _path: &str) -> std::path::PathBuf {
    use std::os::fd::AsRawFd as _;
    std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Unreachable in practice: the upgrade only runs where direct I/O is
/// supported, which is Linux only. Present so the branch compiles elsewhere.
#[cfg(not(target_os = "linux"))]
fn descriptor_path(_file: &tokio::fs::File, path: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(path)
}

/// Whether the direct-I/O upgrade could not be *attempted* at all: no `/proc`
/// to address the descriptor through, or the file unlinked out from under it.
///
/// This is not the caller's open failing — that one has already succeeded, and
/// the file it produced is exactly what was asked for. It is the optional
/// upgrade being unavailable, which is the same answer as the filesystem
/// refusing `O_DIRECT`: keep the buffered handle. Anything else (a descriptor
/// limit, say) is a real problem and is propagated.
#[cfg(unix)]
fn is_upgrade_unreachable(error: &io::Error) -> bool {
    let code = error.raw_os_error();
    code == Some(libc::ENOENT) || code == Some(libc::ENOSYS) || code == Some(libc::EACCES)
}

#[cfg(not(unix))]
fn is_upgrade_unreachable(_error: &io::Error) -> bool {
    false
}

/// Options for the direct-I/O *upgrade* of a file that is already open: the
/// same access, none of the lifecycle.
///
/// `create`, `create_new` and `truncate` are deliberately dropped. The
/// buffered open has already applied them exactly once, and repeating them
/// could clobber the file it just created or fail against its own work.
fn upgrade_options(options: &OpenOptions) -> OpenOptions {
    OpenOptions::new()
        .read(options.is_read())
        .write(options.is_write())
        .append(options.is_append())
}

/// Open `path`, honoring the requested direct-I/O policy.
///
/// Returns the file and whether direct I/O was actually achieved.
///
/// # How `Optional` avoids changing what the open means
///
/// The file's *lifecycle* — `create`, `create_new`, `truncate` — happens exactly
/// once, through a single buffered open carrying the caller's options
/// verbatim, and that open alone decides whether the call succeeds. Direct
/// I/O is then attempted as an **upgrade**: a second open of the file that now
/// exists, with no lifecycle flags of its own, which therefore cannot create,
/// truncate, or clobber anything.
///
/// The upgrade addresses the file by *descriptor* (see [`descriptor_path`]),
/// not by name, so it cannot reopen a different file that a concurrent rename
/// put at the path in the meantime.
///
/// The alternative — attempt `O_DIRECT` first and retry buffered on failure —
/// cannot be made correct. Linux creates the file *before* rejecting
/// `O_DIRECT`, so a failed `create_new` attempt leaves the file behind and the
/// retry then either fails `AlreadyExists` against its own work or has to drop
/// the exclusivity the caller asked for. Neither is `create_new`.
///
/// Only [`is_direct_io_unsupported`] failures of the upgrade are absorbed;
/// every other error is returned, so `Optional` never hides a real problem.
async fn open_file(path: &str, options: &OpenOptions) -> io::Result<OpenedFile> {
    match options.requested_direct_io() {
        DirectIo::Disabled => Ok(OpenedFile::buffered(
            open_options(options, false).open(path).await?,
        )),
        DirectIo::Required if !DIRECT_IO_SUPPORTED => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "direct I/O is not supported on this platform",
        )),
        DirectIo::Required => {
            let file = match open_options(options, true).open(path).await {
                Ok(file) => file,
                // Never downgraded: the caller asked for uncached I/O, and it
                // is not available for this file.
                Err(error) if is_direct_io_unsupported(&error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("direct I/O is not supported for '{path}': {error}"),
                    ));
                }
                Err(error) => return Err(error),
            };
            match direct_io_constraints(&file) {
                Some(constraints) => Ok(OpenedFile::direct(file, constraints)),
                // Better to fail than to advertise constraints that might not
                // be enough: a caller that trusted them would be handed a
                // buffer the device rejects.
                None => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("cannot determine the alignment direct I/O requires for '{path}'"),
                )),
            }
        }
        DirectIo::Optional => {
            let buffered = open_options(options, false).open(path).await?;
            if !DIRECT_IO_SUPPORTED {
                return Ok(OpenedFile::buffered(buffered));
            }
            // Addressed by descriptor, not by name: the upgrade must reopen
            // the file whose lifecycle was just applied, not whatever the path
            // names now.
            let upgrade = descriptor_path(&buffered, path);
            match open_options(&upgrade_options(options), true)
                .open(&upgrade)
                .await
            {
                Ok(direct) => match direct_io_constraints(&direct) {
                    Some(constraints) => Ok(OpenedFile::direct(direct, constraints)),
                    // Uncached I/O whose requirements cannot be described is
                    // worse than buffered I/O that needs none: keep the handle
                    // that is already open.
                    None => Ok(OpenedFile::buffered(buffered)),
                },
                Err(error)
                    if is_direct_io_unsupported(&error) || is_upgrade_unreachable(&error) =>
                {
                    Ok(OpenedFile::buffered(buffered))
                }
                Err(error) => Err(error),
            }
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

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::descriptor_path;
    use super::{OpenOptions, is_direct_io_unsupported, upgrade_options};
    use std::io;

    /// Only the two codes that mean "this filesystem cannot do direct I/O"
    /// may be absorbed into a buffered fallback. Everything else is a real
    /// failure of the open.
    #[cfg(unix)]
    #[test]
    fn only_unsupported_errors_are_absorbed() {
        assert!(is_direct_io_unsupported(&io::Error::from_raw_os_error(
            libc::EINVAL
        )));
        assert!(is_direct_io_unsupported(&io::Error::from_raw_os_error(
            libc::EOPNOTSUPP
        )));

        for code in [
            libc::EACCES,
            libc::EPERM,
            libc::EEXIST,
            libc::ENOENT,
            libc::EMFILE,
            libc::ENFILE,
            libc::EROFS,
            libc::EISDIR,
            libc::ENOSPC,
            libc::ELOOP,
            libc::ENAMETOOLONG,
        ] {
            let error = io::Error::from_raw_os_error(code);
            assert!(
                !is_direct_io_unsupported(&error),
                "{error} must not be turned into a buffered fallback"
            );
        }

        // An error with no OS code behind it says nothing about direct I/O.
        assert!(!is_direct_io_unsupported(&io::Error::other("synthetic")));
    }

    /// The upgrade reopens the file the descriptor holds, not the name it was
    /// opened under. Renaming the name away leaves the descriptor path
    /// resolving to the same file, which is what stops a concurrent rename
    /// from substituting a different one for the file `create_new` just
    /// exclusively created.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_upgrade_addresses_the_file_not_the_name() {
        use std::os::unix::fs::MetadataExt as _;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build runtime");
        runtime.block_on(async {
            let dir = tempfile::TempDir::new().expect("temp dir");
            let original = dir.path().join("original");
            let renamed = dir.path().join("renamed");

            let file = tokio::fs::File::create(&original)
                .await
                .expect("create failed");
            let inode = file.metadata().await.expect("metadata failed").ino();

            // Somebody moves the name out from under us.
            std::fs::rename(&original, &renamed).expect("rename failed");
            assert!(
                std::fs::metadata(&original).is_err(),
                "the original name no longer resolves"
            );

            // The descriptor path still reaches the file it always held.
            let through_descriptor = descriptor_path(&file, "unused");
            let reached = std::fs::metadata(&through_descriptor)
                .expect("the descriptor path must still resolve");
            assert_eq!(
                reached.ino(),
                inode,
                "the upgrade must reopen the same file, not whatever the name points at"
            );
        });
    }

    /// The upgrade open carries the access the caller asked for and none of
    /// the lifecycle: repeating `create_new` or `truncate` against the file
    /// the buffered open just made is exactly the bug this avoids.
    #[test]
    fn the_upgrade_open_drops_every_lifecycle_flag() {
        let requested = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .create_new(true)
            .truncate(true)
            .append(true);
        let upgrade = upgrade_options(&requested);

        assert!(upgrade.is_read());
        assert!(upgrade.is_write());
        assert!(upgrade.is_append());
        assert!(!upgrade.is_create());
        assert!(!upgrade.is_create_new());
        assert!(!upgrade.is_truncate());
    }
}
