//! The open policy both real-filesystem backends share: direct I/O
//! (`Disabled`, `Optional`, `Required`), discovering the alignment it needs,
//! and reopening the file a descriptor already holds.
//!
//! The policy is subtle, and the Tokio and compio backends must answer every
//! open alike, so it is written once here, generic over [`NativeFs`]: the
//! one operation each runtime supplies is "open this path with these flags".

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use super::{DirectIo, IoConstraints, OpenOptions};

/// What a runtime's file type must offer the shared policy: a descriptor to
/// query (alignment, identity, descriptor path).
#[cfg(unix)]
pub(super) trait Descriptor: std::os::fd::AsFd {}

#[cfg(unix)]
impl<T: std::os::fd::AsFd> Descriptor for T {}

/// What a runtime's file type must offer the shared policy: nothing, off
/// Unix, where none of the descriptor queries exist.
#[cfg(not(unix))]
pub(super) trait Descriptor {}

#[cfg(not(unix))]
impl<T> Descriptor for T {}

/// One runtime's way to open and truncate a file.
pub(super) trait NativeFs {
    /// The runtime's open file.
    type File: Descriptor;

    /// Open `path` with exactly `options`, adding `O_DIRECT` when `direct`
    /// (on Linux; elsewhere `direct` is never asked for).
    fn open(
        &self,
        path: &Path,
        options: &OpenOptions,
        direct: bool,
    ) -> impl Future<Output = io::Result<Self::File>>;

    /// Truncate `file` to zero length.
    fn truncate(file: &Self::File) -> impl Future<Output = io::Result<()>>;
}

/// Whether this build can ask the kernel for uncached I/O at all.
pub(super) const DIRECT_IO_SUPPORTED: bool = cfg!(target_os = "linux");

/// Whether an open failure means *direct I/O is unavailable here*, as opposed
/// to the open failing on its own merits.
///
/// Linux rejects `O_DIRECT` on a filesystem that cannot provide it with
/// `EINVAL`; some stacked filesystems answer `EOPNOTSUPP`. Every other error —
/// `EACCES`, `EEXIST`, `ENOENT`, `EMFILE`, `EROFS`, … — is a real failure of
/// the open and must reach the caller unchanged, because falling back to a
/// buffered open would turn it into a success with different semantics.
pub(super) fn is_direct_io_unsupported(error: &io::Error) -> bool {
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
pub(super) struct OpenedFile<F> {
    pub(super) file: F,
    /// What this open requires of callers: [`IoConstraints::NONE`] for a
    /// buffered file, the device's discovered requirement for a direct one.
    pub(super) constraints: IoConstraints,
    /// Whether the open actually got uncached I/O.
    pub(super) direct: bool,
}

impl<F> OpenedFile<F> {
    fn buffered(file: F) -> Self {
        Self {
            file,
            constraints: IoConstraints::NONE,
            direct: false,
        }
    }

    fn direct(file: F, constraints: IoConstraints) -> Self {
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
fn reported_alignment<F: Descriptor>(file: &F) -> ReportedAlignment {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `statx` fills a caller-allocated `struct statx`; the value is
    // zeroed first so every field is initialized whatever the kernel writes.
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open for the duration of the call, the path is
    // an empty C string paired with `AT_EMPTY_PATH` (the documented way to
    // stat a descriptor), and `stx` is a valid, correctly sized output buffer.
    let result = unsafe {
        libc::statx(
            file.as_fd().as_raw_fd(),
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
fn reported_alignment<F>(_file: &F) -> ReportedAlignment {
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
pub(super) fn direct_io_constraints<F: Descriptor>(file: &F) -> Option<IoConstraints> {
    match reported_alignment(file) {
        ReportedAlignment::Known(constraints) => Some(constraints),
        ReportedAlignment::Refused => None,
        ReportedAlignment::Unknown => bounded_alignment(),
    }
}

/// Open `file` — the file this descriptor already holds — a second time with
/// `options`, never whatever `path` happens to name by now.
///
/// Where the system has descriptor paths (Linux with `/proc` mounted) the
/// reopen goes through [`descriptor_path`] and cannot pick up another file.
/// Elsewhere it has to go by name, so on Unix it then checks that both
/// descriptors hold the same inode (device and inode number) and fails with
/// [`io::ErrorKind::Other`] if a concurrent rename replaced the file between
/// the two opens: one handle must never silently read and write two files.
///
/// On a platform that is neither (Windows), the standard library exposes no
/// stable file identity, so the reopen by name is unchecked. That is a known
/// limitation: a rename over `path` racing the open itself can leave the
/// stream and positioned sides of one append handle on different files.
pub(super) async fn reopen_same_file<N: NativeFs>(
    fs: &N,
    file: &N::File,
    path: &str,
    options: &OpenOptions,
    direct: bool,
) -> io::Result<N::File> {
    if descriptor_paths_available() {
        return fs.open(&descriptor_path(file, path), options, direct).await;
    }
    let reopened = fs.open(Path::new(path), options, direct).await?;
    if !same_file(file, &reopened)? {
        return Err(io::Error::other(format!(
            "{path} was replaced while it was being opened; \
             the positioned descriptor would address a different file"
        )));
    }
    Ok(reopened)
}

/// Whether two descriptors hold the same file (device and inode number).
///
/// Asked of a duplicate of each descriptor, so it works for any runtime's
/// file type without knowing its metadata API.
#[cfg(unix)]
fn same_file<A: Descriptor, B: Descriptor>(a: &A, b: &B) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let identity = |file: std::os::fd::BorrowedFd<'_>| -> io::Result<(u64, u64)> {
        let metadata = std::fs::File::from(file.try_clone_to_owned()?).metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    };
    Ok(identity(a.as_fd())? == identity(b.as_fd())?)
}

/// No stable file identity outside Unix: see [`reopen_same_file`].
#[cfg(not(unix))]
fn same_file<A, B>(_a: &A, _b: &B) -> io::Result<bool> {
    Ok(true)
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
pub(super) fn descriptor_path<F: Descriptor>(file: &F, _path: &str) -> PathBuf {
    use std::os::fd::AsRawFd as _;
    PathBuf::from(format!("/proc/self/fd/{}", file.as_fd().as_raw_fd()))
}

/// Never called: every caller first checks [`descriptor_paths_available`],
/// which is `false` off Linux. Present so those branches compile elsewhere.
#[cfg(not(target_os = "linux"))]
pub(super) fn descriptor_path<F>(_file: &F, path: &str) -> PathBuf {
    PathBuf::from(path)
}

/// Whether this system has the descriptor path the `Optional` upgrade
/// reopens through.
///
/// Checked once, by asking whether `/proc/self/fd` is there at all, rather
/// than by guessing from an error code afterwards. A machine without it simply
/// never attempts the upgrade, so no error has to be interpreted as "the
/// mechanism is missing".
#[cfg(target_os = "linux")]
pub(super) fn descriptor_paths_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| std::fs::metadata("/proc/self/fd").is_ok())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn descriptor_paths_available() -> bool {
    false
}

/// The caller's access with **no lifecycle at all**: the shape that opens a
/// file which already exists without being able to create or truncate one.
///
/// Used twice. A `Required` open reaches an existing file with it, so a
/// refused `O_DIRECT` cannot have created or truncated anything. The
/// direct-I/O *upgrade* of an `Optional` open uses it because `create`,
/// `create_new` and `truncate` must not be repeated: the buffered open has
/// already applied them exactly once, and repeating them could clobber the
/// file it just created or fail against its own work.
pub(super) fn access_only(options: &OpenOptions) -> OpenOptions {
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
pub(super) async fn open_file<N: NativeFs>(
    fs: &N,
    path: &str,
    options: &OpenOptions,
) -> io::Result<OpenedFile<N::File>> {
    let named = Path::new(path);
    match options.requested_direct_io() {
        DirectIo::Disabled => Ok(OpenedFile::buffered(fs.open(named, options, false).await?)),
        DirectIo::Required if !DIRECT_IO_SUPPORTED => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "direct I/O is not supported on this platform",
        )),
        DirectIo::Required => open_required(fs, path, options).await,
        DirectIo::Optional => {
            let buffered = fs.open(named, options, false).await?;
            if !DIRECT_IO_SUPPORTED || !descriptor_paths_available() {
                return Ok(OpenedFile::buffered(buffered));
            }
            // Addressed by descriptor, not by name: the upgrade must reopen
            // the file whose lifecycle was just applied, not whatever the path
            // names now.
            let upgrade = descriptor_path(&buffered, path);
            match fs.open(&upgrade, &access_only(options), true).await {
                Ok(direct) => match direct_io_constraints(&direct) {
                    Some(constraints) => Ok(OpenedFile::direct(direct, constraints)),
                    // Uncached I/O whose requirements cannot be described is
                    // worse than buffered I/O that needs none: keep the handle
                    // that is already open.
                    None => Ok(OpenedFile::buffered(buffered)),
                },
                // The only absorbed failure is "no direct I/O here". Whether
                // the mechanism itself exists was settled before the attempt,
                // so nothing else has to be guessed at from an error code.
                Err(error) if is_direct_io_unsupported(&error) => {
                    Ok(OpenedFile::buffered(buffered))
                }
                Err(error) => Err(error),
            }
        }
    }
}

/// Open with mandatory direct I/O.
///
/// `Required` means exactly one thing: *if this file can be opened for direct
/// I/O with the requested access, return a direct-I/O file; otherwise fail.*
/// It never falls back to buffered I/O, and it never creates a file.
///
/// # An existing file
///
/// 1. Open it **as it already is** — the caller's access, `O_DIRECT`, and no
///    lifecycle flags whatever.
/// 2. Discover and validate the alignment direct I/O needs.
/// 3. Only then apply `truncate`, through the descriptor.
///
/// The order is deliberate: the capability check comes first so that a
/// refusal cannot have modified the file. Whether a given kernel would have
/// truncated anyway is not something to depend on — measured on Linux 6.18,
/// `O_TRUNC | O_DIRECT` against a filesystem that refuses `O_DIRECT` leaves
/// the contents intact, but `O_CREAT | O_DIRECT` leaves the file behind (see
/// below), so the ordering of lifecycle against capability is a kernel detail
/// and not a contract.
///
/// # A missing file
///
/// Refused with [`io::ErrorKind::Unsupported`]. Creating a file *and*
/// guaranteeing direct I/O on it is not one filesystem operation, and the
/// provider does not invent one. The native open makes that concrete: a
/// refused `O_CREAT | O_DIRECT` still leaves the created file behind, because
/// the file is created during path resolution and `O_DIRECT` is rejected
/// afterwards. Cleaning that up safely is a namespace protocol — stage an
/// inode elsewhere, publish the name — not a file open. Whoever owns the
/// file's format owns that protocol, and only they know whether the name has
/// to be durable, what a half-created file means, and how recovery finds it.
///
/// The caller bootstraps instead, deliberately and in its own order:
///
/// 1. create the file with an ordinary open,
/// 2. `sync_all` it, and `sync_dir` its parent if the name has to survive a
///    crash,
/// 3. reopen it with `DirectIo::Required`,
/// 4. format and recover.
///
/// A journal or pager does all four anyway; the provider would only be
/// guessing at steps 2 and 4 on its behalf.
async fn open_required<N: NativeFs>(
    fs: &N,
    path: &str,
    options: &OpenOptions,
) -> io::Result<OpenedFile<N::File>> {
    match fs.open(Path::new(path), &access_only(options), true).await {
        Ok(file) => finish_existing::<N>(file, options).await,
        Err(error) if is_direct_io_unsupported(&error) => Err(unsupported_direct_io(path, &error)),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && (options.is_create() || options.is_create_new()) =>
        {
            Err(creation_needs_a_bootstrap(path))
        }
        Err(error) => Err(error),
    }
}

/// Finish a `Required` open of a file that already existed: refuse it if the
/// caller demanded exclusivity, then apply the truncation the open flags were
/// not allowed to carry.
async fn finish_existing<N: NativeFs>(
    file: N::File,
    options: &OpenOptions,
) -> io::Result<OpenedFile<N::File>> {
    if options.is_create_new() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the file already exists",
        ));
    }
    let Some(constraints) = direct_io_constraints(&file) else {
        // Reported before anything is truncated: the file is exactly as the
        // caller left it.
        return Err(undescribable_direct_io("the opened file"));
    };
    if options.is_truncate() {
        N::truncate(&file).await?;
    }
    Ok(OpenedFile::direct(file, constraints))
}

fn unsupported_direct_io(path: &str, error: &io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("direct I/O is not supported for '{path}': {error}"),
    )
}

/// `Required` was asked to create a file. See [`open_required`] for why the
/// provider refuses rather than doing it.
fn creation_needs_a_bootstrap(path: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "'{path}' does not exist: direct I/O cannot be required of a file this open would \
             have to create. Create it with an ordinary open, sync it (and its directory, if \
             the name must be durable), then reopen it with DirectIo::Required."
        ),
    )
}

fn undescribable_direct_io(path: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("cannot determine the alignment direct I/O requires for '{path}'"),
    )
}

/// Reserve storage for `file`'s first `len` bytes with `fallocate(2)` mode
/// 0 (the size grows, never shrinks). `Ok(false)` when the filesystem
/// cannot allocate (`EOPNOTSUPP`): the caller then only grows the file.
///
/// A metadata call on an already-open descriptor; `ENOSPC` comes back as
/// [`io::ErrorKind::StorageFull`].
#[cfg(target_os = "linux")]
pub(super) fn fallocate<F: Descriptor>(file: &F, len: u64) -> io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    let length = libc::off_t::try_from(len)
        .map_err(|_| super::invalid_input(format!("length {len} overflows off_t")))?;
    loop {
        // SAFETY: `fallocate` only reads its integer arguments, and the
        // descriptor stays open for the duration of the call.
        let done = unsafe { libc::fallocate(file.as_fd().as_raw_fd(), 0, 0, length) };
        if done == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EOPNOTSUPP) => return Ok(false),
            _ => return Err(error),
        }
    }
}

/// No allocation call off Linux: the caller only grows the file.
#[cfg(not(target_os = "linux"))]
pub(super) fn fallocate<F: Descriptor>(_file: &F, _len: u64) -> io::Result<bool> {
    Ok(false)
}

/// Drop the clean cached pages of `file` (`posix_fadvise(DONTNEED)` over
/// the whole file): the next read comes from the device.
#[cfg(target_os = "linux")]
pub(super) fn drop_cache<F: Descriptor>(file: &F) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `posix_fadvise` only reads its integer arguments, and the
    // descriptor stays open for the duration of the call.
    let done =
        unsafe { libc::posix_fadvise(file.as_fd().as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    // It returns the error number, it does not set `errno`.
    if done == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(done))
    }
}

/// No page-cache advice off Linux: nothing is dropped.
#[cfg(not(target_os = "linux"))]
pub(super) fn drop_cache<F: Descriptor>(_file: &F) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::descriptor_path;
    use super::{OpenOptions, access_only, is_direct_io_unsupported};
    use std::io;

    /// Only the two codes that mean "this filesystem cannot do direct I/O"
    /// may be absorbed into a buffered fallback. Everything else is a real
    /// failure of the open — a denial in particular is a real answer about
    /// this process's rights, and swallowing it would let a security policy
    /// silently downgrade a caller.
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
            libc::EIO,
            libc::ENOSYS,
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

    /// The descriptor path keeps working after the file is unlinked, which is
    /// why the upgrade needs no "the file is gone" fallback.
    ///
    /// `/proc/self/fd/N` resolves to the inode, not to the name, so a
    /// descriptor whose file has been removed still reopens. That leaves no
    /// reachable condition under which the upgrade fails for want of a path,
    /// and therefore nothing beyond "no direct I/O here" for the fallback to
    /// absorb. Pinned here because the classification depends on it.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_descriptor_path_survives_unlinking() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("unlinked");

        let file = std::fs::File::create(&path).expect("create failed");
        std::fs::remove_file(&path).expect("unlink failed");
        assert!(std::fs::metadata(&path).is_err(), "the name is gone");

        std::fs::OpenOptions::new()
            .read(true)
            .open(descriptor_path(&file, "unused"))
            .expect("the descriptor path still reaches the unlinked file");
    }

    /// The access-only shape carries the access the caller asked for and none
    /// of the lifecycle. `Required` opens the existing file with it, so a
    /// rejected `O_DIRECT` cannot create or truncate anything on the way out;
    /// the `Optional` upgrade uses it because repeating `create_new` or
    /// `truncate` against the file the buffered open just made is exactly the
    /// bug it avoids.
    #[test]
    fn the_access_only_open_drops_every_lifecycle_flag() {
        let requested = OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .create(true)
            .create_new(true)
            .truncate(true);
        let access = access_only(&requested);

        assert!(access.is_read());
        assert!(access.is_write());
        assert!(access.is_append());
        assert!(!access.is_create());
        assert!(!access.is_create_new());
        assert!(!access.is_truncate());
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

        let dir = tempfile::TempDir::new().expect("temp dir");
        let original = dir.path().join("original");
        let renamed = dir.path().join("renamed");

        let file = std::fs::File::create(&original).expect("create failed");
        let inode = file.metadata().expect("metadata failed").ino();

        // Somebody moves the name out from under us.
        std::fs::rename(&original, &renamed).expect("rename failed");
        assert!(
            std::fs::metadata(&original).is_err(),
            "the original name no longer resolves"
        );

        // The descriptor path still reaches the file it always held.
        let through_descriptor = descriptor_path(&file, "unused");
        let reached =
            std::fs::metadata(&through_descriptor).expect("the descriptor path must still resolve");
        assert_eq!(
            reached.ino(),
            inode,
            "the upgrade must reopen the same file, not whatever the name points at"
        );
    }
}
