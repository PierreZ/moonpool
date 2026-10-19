//! The direct-I/O open protocol's host-side pieces, for storage backends.
//!
//! A production [`StorageProvider`](crate::StorageProvider) honours
//! [`DirectIo`](crate::DirectIo) by opening files with `O_DIRECT`, probing
//! the alignment the kernel then demands, and classifying the errors an open
//! can fail with. Those steps depend only on a raw descriptor, `libc`,
//! [`OpenOptions`] and [`IoConstraints`], so every backend (Tokio's, compio's)
//! shares this one copy instead of carrying its own. The open itself, the
//! `Optional` upgrade and the `Required` refusal rules stay in each backend,
//! because they are written against that runtime's file type.
//!
//! These are host-side helpers shared by the production storage backends,
//! not part of the provider API: that is why they sit at the crate root
//! behind the `host` feature, apart from the storage traits and types.
//!
//! The descriptor-taking helpers accept any `F: AsFd` on unix. Elsewhere
//! they take any `&F` and ignore it, so a backend's call sites compile
//! unchanged on every target.

use std::io;
#[cfg(unix)]
use std::os::fd::AsFd;

use super::{IoConstraints, OpenOptions};

/// Whether this build can ask the kernel for uncached I/O at all.
pub const DIRECT_IO_SUPPORTED: bool = cfg!(target_os = "linux");

/// Whether an open failure means *direct I/O is unavailable here*, as opposed
/// to the open failing on its own merits.
///
/// Linux rejects `O_DIRECT` on a filesystem that cannot provide it with
/// `EINVAL`; some stacked filesystems answer `EOPNOTSUPP`. Every other error
/// (`EACCES`, `EEXIST`, `ENOENT`, `EMFILE`, `EROFS`, …) is a real failure of
/// the open and must reach the caller unchanged, because falling back to a
/// buffered open would turn it into a success with different semantics.
#[must_use]
pub fn is_direct_io_unsupported(error: &io::Error) -> bool {
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

/// What the kernel will say about a file's direct-I/O alignment.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
enum ReportedAlignment {
    /// The kernel described this file's actual requirement.
    Known(IoConstraints),
    /// The kernel says this file cannot do direct I/O at all.
    Refused,
    /// The kernel would not say: an older kernel, or a libc without the
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
fn reported_alignment<F: AsFd>(file: &F) -> ReportedAlignment {
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

/// An alignment guaranteed to satisfy this system's direct I/O, for kernels
/// that will not report the real one.
///
/// The page size is that bound on Linux. Direct-I/O alignment derives from the
/// underlying device's logical block size, and the block layer refuses a
/// logical block size larger than a page; the kernels that lifted that
/// restriction are also the kernels that report `STATX_DIOALIGN`, which is
/// preferred whenever it is available. So the page size is never too small,
/// and being larger than the device needs only narrows what callers may do,
/// which is the direction [`IoConstraints`] is allowed to err in.
#[cfg(unix)]
#[must_use]
pub fn bounded_alignment() -> Option<IoConstraints> {
    // SAFETY: `sysconf` reads a system parameter and has no preconditions.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = usize::try_from(page_size).ok()?;
    if page_size == 0 || !page_size.is_power_of_two() {
        return None;
    }
    Some(IoConstraints::uniform(page_size))
}

/// No bound outside unix: direct I/O is never offered there.
#[cfg(not(unix))]
#[must_use]
pub fn bounded_alignment() -> Option<IoConstraints> {
    None
}

/// The constraints a direct-I/O open must advertise, or `None` if this file
/// cannot do direct I/O or nothing can bound what it would require.
///
/// Advertising constraints that are too weak would hand the caller an aligned
/// buffer the device then rejects, so "we do not know" is answered by not
/// offering direct I/O rather than by guessing.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[must_use]
pub fn direct_io_constraints<F: AsFd>(file: &F) -> Option<IoConstraints> {
    match reported_alignment(file) {
        ReportedAlignment::Known(constraints) => Some(constraints),
        ReportedAlignment::Refused => None,
        ReportedAlignment::Unknown => bounded_alignment(),
    }
}

/// The kernel reports nothing here, so the bound is all there is.
#[cfg(all(unix, not(all(target_os = "linux", target_env = "gnu"))))]
#[must_use]
pub fn direct_io_constraints<F: AsFd>(_file: &F) -> Option<IoConstraints> {
    bounded_alignment()
}

/// Outside unix the kernel reports nothing and nothing bounds the
/// requirement, so this is always `None`.
#[cfg(not(unix))]
#[must_use]
pub fn direct_io_constraints<F: ?Sized>(_file: &F) -> Option<IoConstraints> {
    None
}

/// The path that reopens *this exact file*, rather than whatever the original
/// path happens to name by the time the upgrade runs.
///
/// `/proc/self/fd/N` is a magic symlink to the file a descriptor holds, so
/// reopening through it cannot pick up a different file. Reopening by name
/// could: a concurrent rename between the two opens would hand back somebody
/// else's file, and `create_new`'s guarantee (that the caller holds the file
/// it exclusively created) would last only until someone renamed over the
/// name.
#[cfg(target_os = "linux")]
#[must_use]
pub fn descriptor_path<F: AsFd>(file: &F, _path: &str) -> std::path::PathBuf {
    use std::os::fd::AsRawFd as _;
    std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_fd().as_raw_fd()))
}

/// Never called: every caller first checks [`descriptor_paths_available`],
/// which is `false` off Linux. Present so those branches compile elsewhere.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn descriptor_path<F: ?Sized>(_file: &F, path: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(path)
}

/// Whether this system has the descriptor path the `Optional` upgrade
/// reopens through.
///
/// Checked once, by asking whether `/proc/self/fd` is there at all, rather
/// than by guessing from an error code afterwards. A machine without it simply
/// never attempts the upgrade, so no error has to be interpreted as "the
/// mechanism is missing".
#[cfg(target_os = "linux")]
#[must_use]
pub fn descriptor_paths_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| std::fs::metadata("/proc/self/fd").is_ok())
}

/// No descriptor paths off Linux: the upgrade is never attempted.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn descriptor_paths_available() -> bool {
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
#[must_use]
pub fn access_only(options: &OpenOptions) -> OpenOptions {
    OpenOptions::new()
        .read(options.is_read())
        .write(options.is_write())
        .append(options.is_append())
}

/// The error a `Required` open returns when the kernel refused `O_DIRECT`
/// for `path` (`error` is the refusal).
#[must_use]
pub fn unsupported_direct_io(path: &str, error: &io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("direct I/O is not supported for '{path}': {error}"),
    )
}

/// `Required` was asked to create a file.
///
/// Creating a file *and* guaranteeing direct I/O on it is not one filesystem
/// operation: a refused `O_CREAT | O_DIRECT` still leaves the created file
/// behind. The provider refuses rather than doing it, and the message tells
/// the caller how to bootstrap the file instead.
#[must_use]
pub fn creation_needs_a_bootstrap(path: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "'{path}' does not exist: direct I/O cannot be required of a file this open would \
             have to create. Create it with an ordinary open, sync it (and its directory, if \
             the name must be durable), then reopen it with DirectIo::Required."
        ),
    )
}

/// The error for a direct-I/O file whose alignment
/// [`direct_io_constraints`] could not determine.
#[must_use]
pub fn undescribable_direct_io(path: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("cannot determine the alignment direct I/O requires for '{path}'"),
    )
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::descriptor_path;
    use super::{OpenOptions, access_only, is_direct_io_unsupported};
    use std::io;

    /// Only the two codes that mean "this filesystem cannot do direct I/O"
    /// may be absorbed into a buffered fallback. Everything else is a real
    /// failure of the open: a denial in particular is a real answer about
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
