//! Compio [`LocalStorageProvider`] / [`LocalStorageFile`] over the real
//! filesystem.
//!
//! Every positioned transfer, sync and size query is one completion-based
//! operation on compio's driver: `io_uring` on Linux (polling where the kernel
//! refuses `io_uring`), IOCP on Windows. Nothing goes through a blocking
//! thread pool except the calls compio has no operation for: listing a
//! directory, `fallocate` ([`LocalStorageFile::allocate`]) and
//! `posix_fadvise` ([`LocalStorageFile::drop_cache`]). Those can block for a
//! long time, so they must not stop the runtime thread.

use std::io;

use compio::buf::BufResult;
use compio::io::{AsyncReadAt, AsyncWriteAt};

use super::native::{self, NativeFs, OpenedFile};
use super::{AlignedBuf, IoConstraints, LocalStorageFile, LocalStorageProvider, OpenOptions};

/// Compio storage provider: files on the real filesystem, driven by compio.
///
/// It implements [`LocalStorageProvider`] only. Compio files live on the
/// thread's runtime (each holds an `Rc` to its descriptor) and its operations
/// are not `Send`, so it cannot implement
/// [`StorageProvider`](super::StorageProvider). Every method needs a running
/// compio runtime.
///
/// ## Direct I/O
///
/// Opens follow exactly the policy of
/// [`TokioStorageProvider`](super::TokioStorageProvider), from the same code:
/// [`DirectIo::Optional`](super::DirectIo::Optional) upgrades to `O_DIRECT`
/// where the filesystem allows it, [`DirectIo::Required`](super::DirectIo::Required)
/// fails rather than fall back and never creates a file, and a direct file
/// advertises the alignment the kernel reports (`statx`), or the page size
/// when it will not say. Direct I/O is Linux only, as there.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompioStorageProvider;

impl CompioStorageProvider {
    /// Create a compio storage provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Compio's half of the shared open policy (see [`native`]).
#[derive(Debug, Clone, Copy)]
struct CompioFs;

impl NativeFs for CompioFs {
    type File = compio::fs::File;

    async fn open(
        &self,
        path: &std::path::Path,
        options: &OpenOptions,
        direct: bool,
    ) -> io::Result<compio::fs::File> {
        let mut builder = compio::fs::OpenOptions::new();
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
        builder.open(path).await
    }

    async fn truncate(file: &compio::fs::File) -> io::Result<()> {
        file.set_len(0).await
    }
}

impl LocalStorageProvider for CompioStorageProvider {
    type File = CompioStorageFile;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        let opened = native::open_file(&CompioFs, path, &options).await?;
        let OpenedFile {
            file,
            constraints,
            direct,
        } = opened;
        Ok(CompioStorageFile {
            file,
            constraints,
            direct,
        })
    }

    async fn exists(&self, path: &str) -> io::Result<bool> {
        match compio::fs::metadata(path).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn delete(&self, path: &str) -> io::Result<()> {
        compio::fs::remove_file(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        compio::fs::rename(from, to).await
    }

    async fn create_dir_all(&self, path: &str) -> io::Result<()> {
        compio::fs::create_dir_all(path).await
    }

    async fn list_dir(&self, path: &str) -> io::Result<Vec<String>> {
        let path = path.to_string();
        compio::runtime::spawn_blocking(move || list_dir_blocking(&path))
            .await
            .map_err(|error| io::Error::other(format!("directory listing task failed: {error}")))?
    }

    async fn sync_dir(&self, path: &str) -> io::Result<()> {
        sync_dir_impl(path).await
    }
}

/// Run `work` on compio's blocking pool, on a duplicate of `file`'s
/// descriptor.
///
/// Use it for a syscall that compio has no operation for and that can block
/// for a long time (`fallocate`, `posix_fadvise`): on the runtime thread it
/// would stop every task of the thread. The duplicate is a plain
/// [`std::fs::File`], because a compio file must stay on its runtime thread.
async fn off_runtime<T, F>(file: &compio::fs::File, work: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&std::fs::File) -> io::Result<T> + Send + 'static,
{
    let duplicate = duplicate_descriptor(file)?;
    compio::runtime::spawn_blocking(move || work(&duplicate))
        .await
        .map_err(|error| io::Error::other(format!("blocking file task failed: {error}")))?
}

/// A second descriptor for the open file of `file`, as a [`std::fs::File`].
#[cfg(unix)]
fn duplicate_descriptor(file: &compio::fs::File) -> io::Result<std::fs::File> {
    use std::os::fd::AsFd as _;
    Ok(std::fs::File::from(file.as_fd().try_clone_to_owned()?))
}

/// A second handle for the open file of `file`, as a [`std::fs::File`].
#[cfg(windows)]
fn duplicate_descriptor(file: &compio::fs::File) -> io::Result<std::fs::File> {
    use std::os::windows::io::AsHandle as _;
    Ok(std::fs::File::from(file.as_handle().try_clone_to_owned()?))
}

fn list_dir_blocking(path: &str) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let name = entry?.file_name().into_string().map_err(|name| {
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

/// Fsync a directory so its entries survive a crash: open it read-only and
/// sync it, the portable unix idiom.
#[cfg(unix)]
async fn sync_dir_impl(path: &str) -> io::Result<()> {
    let directory = compio::fs::File::open(path).await?;
    if !directory.metadata().await?.is_dir() {
        return Err(not_a_directory());
    }
    directory.sync_all().await
}

/// Windows has no directory handle to fsync; NTFS journals metadata. As the
/// Tokio backend does, validate the directory and succeed.
#[cfg(not(unix))]
async fn sync_dir_impl(path: &str) -> io::Result<()> {
    if !compio::fs::metadata(path).await?.is_dir() {
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

/// One open file of [`CompioStorageProvider`].
///
/// Every transfer is positioned and takes `&self`. The owned forms
/// (`*_aligned`) hand the caller's [`AlignedBuf`] to the kernel itself; the
/// borrowed forms copy through an aligned buffer of their own, because a
/// completion operation must own its memory until the kernel is done with it.
#[derive(Debug)]
pub struct CompioStorageFile {
    file: compio::fs::File,
    /// What this open requires of callers (see [`IoConstraints`]).
    constraints: IoConstraints,
    /// Whether the open got uncached I/O.
    direct: bool,
}

impl CompioStorageFile {
    /// A bounce buffer for `len` bytes, aligned the way this file's transfers
    /// must be.
    fn bounce(&self, len: usize) -> AlignedBuf {
        AlignedBuf::zeroed(len, self.constraints.memory_alignment())
    }

    /// The owned read: `buf` goes to the kernel and comes back.
    async fn read_owned(&self, offset: u64, buf: AlignedBuf) -> io::Result<(usize, AlignedBuf)> {
        if buf.is_empty() {
            return Ok((0, buf));
        }
        self.constraints.check(offset, buf.as_slice())?;
        let BufResult(result, buf) = self.file.read_at(buf, offset).await;
        Ok((result?, buf))
    }

    /// The owned write: `buf` goes to the kernel and comes back.
    async fn write_owned(&self, offset: u64, buf: AlignedBuf) -> io::Result<(usize, AlignedBuf)> {
        if buf.is_empty() {
            return Ok((0, buf));
        }
        self.constraints.check(offset, buf.as_slice())?;
        let BufResult(result, buf) = (&self.file).write_at(buf, offset).await;
        Ok((result?, buf))
    }
}

impl LocalStorageFile for CompioStorageFile {
    async fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all().await
    }

    async fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data().await
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
        // Checked against the caller's own slice: the contract is theirs,
        // whatever this backend does next.
        self.constraints.check(offset, buf)?;
        let (read, bounce) = self.read_owned(offset, self.bounce(buf.len())).await?;
        buf[..read].copy_from_slice(&bounce.as_slice()[..read]);
        Ok(read)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        let mut bounce = self.bounce(buf.len());
        bounce.as_mut_slice().copy_from_slice(buf);
        let (written, _) = self.write_owned(offset, bounce).await?;
        Ok(written)
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
        self.write_owned(offset, buf).await
    }

    async fn write_at_durable_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> io::Result<(usize, AlignedBuf)> {
        let (written, buf) = self.write_owned(offset, buf).await?;
        self.file.sync_data().await?;
        Ok((written, buf))
    }

    async fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata().await?.len())
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.file.set_len(size).await
    }

    async fn allocate(&self, len: u64) -> io::Result<()> {
        // `fallocate` of a large range can block for a long time.
        let allocated = off_runtime(&self.file, move |file| native::fallocate(file, len)).await?;
        if !allocated && self.size().await? < len {
            self.file.set_len(len).await?;
        }
        Ok(())
    }

    async fn drop_cache(&self) -> io::Result<()> {
        if self.direct {
            return Ok(());
        }
        // `POSIX_FADV_DONTNEED` over a large file can block for a long time.
        off_runtime(&self.file, native::drop_cache).await
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::fd::AsRawFd as _;

    use compio::driver::{DriverType, ProactorBuilder};

    use super::{CompioStorageProvider, LocalStorageFile, LocalStorageProvider};
    use crate::{DirectIo, OpenOptions};

    /// The status flags the kernel actually holds for `file`'s descriptor.
    fn status_flags(file: &compio::fs::File) -> libc::c_int {
        // SAFETY: `F_GETFL` only reads the flags of a descriptor that stays
        // open for the duration of the call.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        assert!(
            flags >= 0,
            "fcntl failed: {}",
            std::io::Error::last_os_error()
        );
        flags
    }

    /// Run `test` on a runtime forced onto `driver`, so the open goes through
    /// that driver's `openat` (an `io_uring` submission, or a plain syscall).
    fn on_driver(driver: DriverType, test: impl AsyncFnOnce()) {
        let mut proactor = ProactorBuilder::new();
        proactor.driver_type(driver);
        let runtime = compio::runtime::RuntimeBuilder::new()
            .with_proactor(proactor)
            .build()
            .expect("compio runtime");
        if runtime.driver_type() != driver {
            eprintln!("skipped: {driver:?} is unavailable here");
            return;
        }
        runtime.block_on(test());
    }

    /// The flags reach the kernel whichever driver opens the file: a direct
    /// file's descriptor really has `O_DIRECT`.
    #[test]
    fn open_flags_reach_the_kernel_on_every_driver() {
        for driver in [DriverType::IoUring, DriverType::Poll] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("flags.db");
            let path = path.to_str().expect("UTF-8 path").to_owned();
            on_driver(driver, async || {
                let storage = CompioStorageProvider::new();
                let file = storage
                    .open(
                        &path,
                        OpenOptions::create_write()
                            .read(true)
                            .direct_io(DirectIo::Optional),
                    )
                    .await
                    .expect("open");
                let direct = status_flags(&file.file) & libc::O_DIRECT != 0;
                assert_eq!(direct, file.is_direct_io(), "{driver:?}");
                if !direct {
                    eprintln!("note: no direct I/O on this filesystem ({driver:?})");
                }
            });
        }
    }

    /// `fallocate` and `posix_fadvise` run on the blocking pool, not on the
    /// runtime thread, and they act on the file: `allocate` grows it, and
    /// `drop_cache` keeps its bytes.
    #[test]
    fn allocate_and_drop_cache_run_off_the_runtime_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("off-runtime.db");
        let path = path.to_str().expect("UTF-8 path").to_owned();
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let storage = CompioStorageProvider::new();
            let file = storage
                .open(&path, OpenOptions::create_write().read(true))
                .await
                .expect("open");

            let runtime_thread = std::thread::current().id();
            let work_thread = super::off_runtime(&file.file, |_| Ok(std::thread::current().id()))
                .await
                .expect("blocking task");
            assert_ne!(work_thread, runtime_thread);

            file.write_at(0, b"keep").await.expect("write");
            file.allocate(1 << 20).await.expect("allocate");
            assert!(file.size().await.expect("size") >= 1 << 20);
            file.drop_cache().await.expect("drop cache");
            let mut back = [0_u8; 4];
            file.read_at(0, &mut back).await.expect("read");
            assert_eq!(&back, b"keep");
        });
    }
}
