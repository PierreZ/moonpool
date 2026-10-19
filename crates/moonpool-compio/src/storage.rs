//! Files on the compio runtime: [`CompioStorageProvider`] and
//! [`CompioStorageFile`].
//!
//! The open protocol (direct-I/O policy, the `Optional` upgrade by
//! descriptor, the `Required` refusal rules, the alignment probe) is a copy
//! of `moonpool-core`'s Tokio backend (`crates/moonpool-core/src/storage/tokio_impl.rs`),
//! whose helpers are private to its `tokio-fs` feature. Keep the two in step:
//! the long-form reasoning lives in the original.

use std::fmt;
use std::future::Future;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

use compio::BufResult;
use compio::fs::File;
use compio::io::{AsyncReadAt, AsyncWriteAt};
use compio_send_wrapper::SendWrapper;
use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use moonpool_core::{
    AlignedBuf, DirectIo, IoConstraints, OpenOptions, StorageFile, StorageProvider,
    stream_io_unsupported,
};

use crate::buf::CompioAlignedBuf;

/// Storage provider over `compio::fs`: on Linux every read, write and sync
/// is an `io_uring` operation when the runtime runs the `io_uring` driver.
///
/// Opens follow the same direct-I/O protocol as `TokioStorageProvider`
/// (see [`DirectIo`]). [`list_dir`](StorageProvider::list_dir) runs
/// `std::fs::read_dir` on compio's blocking pool, since compio has no
/// directory listing.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompioStorageProvider;

impl CompioStorageProvider {
    /// Create a compio storage provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl StorageProvider for CompioStorageProvider {
    type File = CompioStorageFile;

    fn open(
        &self,
        path: &str,
        options: OpenOptions,
    ) -> impl Future<Output = io::Result<Self::File>> + Send {
        SendWrapper::new(open(path.to_owned(), options))
    }

    fn exists(&self, path: &str) -> impl Future<Output = io::Result<bool>> + Send {
        SendWrapper::new(exists(path.to_owned()))
    }

    fn delete(&self, path: &str) -> impl Future<Output = io::Result<()>> + Send {
        SendWrapper::new(delete(path.to_owned()))
    }

    fn rename(&self, from: &str, to: &str) -> impl Future<Output = io::Result<()>> + Send {
        SendWrapper::new(rename(from.to_owned(), to.to_owned()))
    }

    fn create_dir_all(&self, path: &str) -> impl Future<Output = io::Result<()>> + Send {
        SendWrapper::new(create_dir_all(path.to_owned()))
    }

    fn list_dir(&self, path: &str) -> impl Future<Output = io::Result<Vec<String>>> + Send {
        SendWrapper::new(list_dir(path.to_owned()))
    }

    fn sync_dir(&self, path: &str) -> impl Future<Output = io::Result<()>> + Send {
        SendWrapper::new(sync_dir(path.to_owned()))
    }
}

#[tracing::instrument(skip_all, fields(path = %path))]
async fn open(path: String, options: OpenOptions) -> io::Result<CompioStorageFile> {
    let opened = open_file(&path, &options).await?;
    Ok(CompioStorageFile {
        file: opened.file,
        constraints: opened.constraints,
        direct: opened.direct,
        append: options.is_append(),
        cursor: 0,
        in_flight: None,
    })
}

async fn exists(path: String) -> io::Result<bool> {
    match compio::fs::metadata(&path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[tracing::instrument(skip_all, fields(path = %path))]
async fn delete(path: String) -> io::Result<()> {
    compio::fs::remove_file(&path).await
}

#[tracing::instrument(skip_all, fields(from = %from, to = %to))]
async fn rename(from: String, to: String) -> io::Result<()> {
    compio::fs::rename(&from, &to).await
}

#[tracing::instrument(skip_all, fields(path = %path))]
async fn create_dir_all(path: String) -> io::Result<()> {
    compio::fs::create_dir_all(&path).await
}

#[tracing::instrument(skip_all, fields(path = %path))]
async fn list_dir(path: String) -> io::Result<Vec<String>> {
    compio::runtime::spawn_blocking(move || list_dir_blocking(&path))
        .await
        .map_err(|error| io::Error::other(format!("blocking directory listing failed: {error}")))?
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

/// Fsync a directory so its entries survive a crash: open it read-only with
/// `O_DIRECTORY` (a file answers `ENOTDIR`, which is
/// [`io::ErrorKind::NotADirectory`]) and `fsync` it.
#[cfg(unix)]
#[tracing::instrument(skip_all, fields(path = %path))]
async fn sync_dir(path: String) -> io::Result<()> {
    let mut options = compio::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_DIRECTORY);
    let directory = options.open(&path).await?;
    directory.sync_all().await
}

/// No directory handle to fsync outside unix: validate, then succeed (see
/// the Tokio backend for why that is sound on NTFS).
#[cfg(not(unix))]
#[tracing::instrument(skip_all, fields(path = %path))]
async fn sync_dir(path: String) -> io::Result<()> {
    if !compio::fs::metadata(&path).await?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "sync_dir requires a directory",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The open protocol: a copy of the Tokio backend's, over compio's open.
// ---------------------------------------------------------------------------

/// Build compio open options for `options`, optionally adding `O_DIRECT`.
///
/// compio has no append mode. Append implies write access here, and the
/// stream API emulates it (see [`CompioStorageFile`]); the descriptor never
/// carries `O_APPEND`, so a positioned write always lands where it is told.
fn compio_options(options: &OpenOptions, direct: bool) -> compio::fs::OpenOptions {
    let mut builder = compio::fs::OpenOptions::new();
    builder
        .read(options.is_read())
        .write(options.is_write() || options.is_append())
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

/// Whether this build can ask the kernel for uncached I/O at all.
const DIRECT_IO_SUPPORTED: bool = cfg!(target_os = "linux");

/// Whether an open failure means *direct I/O is unavailable here*:
/// `EINVAL` or `EOPNOTSUPP`. Every other error is a real failure of the open.
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
    file: File,
    constraints: IoConstraints,
    direct: bool,
}

impl OpenedFile {
    fn buffered(file: File) -> Self {
        Self {
            file,
            constraints: IoConstraints::NONE,
            direct: false,
        }
    }

    fn direct(file: File, constraints: IoConstraints) -> Self {
        Self {
            file,
            constraints,
            direct: true,
        }
    }
}

/// What the kernel will say about a file's direct-I/O alignment.
enum ReportedAlignment {
    Known(IoConstraints),
    Refused,
    Unknown,
}

/// `STATX_DIOALIGN`, which `libc` does not export.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const STATX_DIOALIGN: libc::c_uint = 0x0000_2000;

/// Ask the kernel what alignment direct I/O on this file requires
/// (`statx(STATX_DIOALIGN)`, Linux 5.19+). A metadata query on an open
/// descriptor, so it runs inline.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn reported_alignment(file: &File) -> ReportedAlignment {
    use std::os::fd::{AsFd as _, AsRawFd as _};

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
        return ReportedAlignment::Unknown;
    }
    ReportedAlignment::Known(IoConstraints::new(offset, length, memory))
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn reported_alignment(_file: &File) -> ReportedAlignment {
    ReportedAlignment::Unknown
}

/// The page size: an alignment guaranteed to satisfy direct I/O on kernels
/// that will not report the real one.
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

/// The constraints a direct-I/O open must advertise, or `None` when this
/// file cannot do direct I/O or nothing can bound what it would require.
fn direct_io_constraints(file: &File) -> Option<IoConstraints> {
    match reported_alignment(file) {
        ReportedAlignment::Known(constraints) => Some(constraints),
        ReportedAlignment::Refused => None,
        ReportedAlignment::Unknown => bounded_alignment(),
    }
}

/// `/proc/self/fd/N`: the path that reopens *this exact file*.
#[cfg(target_os = "linux")]
fn descriptor_path(file: &File) -> std::path::PathBuf {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_fd().as_raw_fd()))
}

/// Whether `/proc/self/fd` is there for the `Optional` upgrade.
#[cfg(target_os = "linux")]
fn descriptor_paths_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| std::fs::metadata("/proc/self/fd").is_ok())
}

/// The caller's access with no lifecycle (`create`, `create_new`,
/// `truncate`) at all.
fn access_only(options: &OpenOptions) -> OpenOptions {
    OpenOptions::new()
        .read(options.is_read())
        .write(options.is_write())
        .append(options.is_append())
}

/// Open `path`, honoring the requested direct-I/O policy. `Optional` applies
/// the lifecycle once through a buffered open, then upgrades by descriptor.
async fn open_file(path: &str, options: &OpenOptions) -> io::Result<OpenedFile> {
    match options.requested_direct_io() {
        DirectIo::Disabled => Ok(OpenedFile::buffered(
            compio_options(options, false).open(path).await?,
        )),
        DirectIo::Required if !DIRECT_IO_SUPPORTED => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "direct I/O is not supported on this platform",
        )),
        DirectIo::Required => open_required(path, options).await,
        DirectIo::Optional => {
            let buffered = compio_options(options, false).open(path).await?;
            upgrade_to_direct(buffered, options).await
        }
    }
}

/// The `Optional` upgrade: reopen the file the buffered open holds, by
/// descriptor, with `O_DIRECT` and no lifecycle flags. Only "no direct I/O
/// here" failures fall back to the buffered handle.
#[cfg(target_os = "linux")]
async fn upgrade_to_direct(buffered: File, options: &OpenOptions) -> io::Result<OpenedFile> {
    if !descriptor_paths_available() {
        return Ok(OpenedFile::buffered(buffered));
    }
    let upgrade = descriptor_path(&buffered);
    match compio_options(&access_only(options), true)
        .open(&upgrade)
        .await
    {
        Ok(direct) => match direct_io_constraints(&direct) {
            Some(constraints) => Ok(OpenedFile::direct(direct, constraints)),
            None => Ok(OpenedFile::buffered(buffered)),
        },
        Err(error) if is_direct_io_unsupported(&error) => Ok(OpenedFile::buffered(buffered)),
        Err(error) => Err(error),
    }
}

#[cfg(not(target_os = "linux"))]
async fn upgrade_to_direct(buffered: File, _options: &OpenOptions) -> io::Result<OpenedFile> {
    Ok(OpenedFile::buffered(buffered))
}

/// Open with mandatory direct I/O: an existing file only, opened as it is,
/// truncated (if asked) only once direct I/O is secured.
async fn open_required(path: &str, options: &OpenOptions) -> io::Result<OpenedFile> {
    match compio_options(&access_only(options), true).open(path).await {
        Ok(file) => finish_existing(file, options).await,
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

async fn finish_existing(file: File, options: &OpenOptions) -> io::Result<OpenedFile> {
    if options.is_create_new() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the file already exists",
        ));
    }
    let Some(constraints) = direct_io_constraints(&file) else {
        return Err(undescribable_direct_io("the opened file"));
    };
    if options.is_truncate() {
        file.set_len(0).await?;
    }
    Ok(OpenedFile::direct(file, constraints))
}

fn unsupported_direct_io(path: &str, error: &io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("direct I/O is not supported for '{path}': {error}"),
    )
}

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

// ---------------------------------------------------------------------------
// The file.
// ---------------------------------------------------------------------------

/// A file opened by [`CompioStorageProvider`].
///
/// **Positioned I/O** is compio's native shape: `read_at` / `write_at` are
/// single `io_uring` operations. The owned forms
/// ([`read_at_aligned`](StorageFile::read_at_aligned) and its siblings) hand
/// the caller's [`AlignedBuf`] to the kernel with no copy; the borrowed forms
/// copy through an aligned bounce buffer, because an in-flight operation must
/// own its buffer.
///
/// **Durable writes** ([`write_at_durable`](StorageFile::write_at_durable)
/// and its owned form) are a write followed by `sync_data`: two operations,
/// a stronger guarantee than the contract asks for. compio has no per-write
/// `RWF_DSYNC`, which the Tokio backend uses for a single durable `pwritev2`;
/// closing that gap needs a custom compio operation.
///
/// **Stream I/O** (on a buffered file only; a direct-I/O file refuses it
/// with [`stream_io_unsupported`]) is emulated over positioned operations
/// and a cursor kept here, since compio files have no cursor. Each
/// `poll_read` / `poll_write` runs one operation through an owned buffer
/// (one copy), and returns once it completed. A file opened with
/// `append(true)` writes each stream write at the file's current size, read
/// just before the write: unlike `O_APPEND`, this is **not atomic** against
/// another writer appending to the same file at the same time.
///
/// Every future this file returns, and the file itself while a stream
/// operation is in flight, is bound to the runtime thread that created it
/// (see the crate documentation).
pub struct CompioStorageFile {
    file: File,
    constraints: IoConstraints,
    direct: bool,
    append: bool,
    /// The stream cursor: where the next stream read or write goes.
    cursor: u64,
    /// The stream operation started by a `poll_*` that returned `Pending`.
    in_flight: Option<InFlight>,
}

impl fmt::Debug for CompioStorageFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompioStorageFile")
            .field("constraints", &self.constraints)
            .field("direct", &self.direct)
            .field("append", &self.append)
            .field("cursor", &self.cursor)
            .field("in_flight", &self.in_flight.as_ref().map(|op| op.kind))
            .finish_non_exhaustive()
    }
}

/// Which stream operation is in flight. A seek remembers its target, so a
/// different seek does not take over its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamOp {
    Read,
    Write,
    Seek(SeekFrom),
}

/// What a stream operation produced.
enum Done {
    /// The bytes read (possibly fewer than asked, none at end of file).
    Read(Vec<u8>),
    /// The written count and the offset just past it.
    Write { written: usize, end: u64 },
    /// The new cursor.
    Seek(u64),
}

type BoxedOp = Pin<Box<dyn Future<Output = io::Result<Done>>>>;

/// A stream operation in flight: a compio future, so thread-bound.
struct InFlight {
    kind: StreamOp,
    op: SendWrapper<BoxedOp>,
}

impl InFlight {
    fn new(kind: StreamOp, op: BoxedOp) -> Self {
        Self {
            kind,
            op: SendWrapper::new(op),
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Done>> {
        self.op
            .get_mut()
            .expect("a compio file was used off the runtime thread that created it")
            .as_mut()
            .poll(cx)
    }
}

fn unexpected_completion() -> io::Error {
    io::Error::other("a stream operation completed with another operation's result")
}

/// Which positioned write an owned transfer performs.
#[derive(Debug, Clone, Copy)]
enum WriteKind {
    Visible,
    Durable,
}

impl CompioStorageFile {
    /// Drive the stream operation `kind` to completion, starting it with
    /// `start(file, cursor)` unless it is already in flight. An in-flight
    /// operation of another kind (one whose `poll_*` caller went away) is
    /// finished first and its effect on the cursor kept.
    fn poll_stream(
        &mut self,
        cx: &mut Context<'_>,
        kind: StreamOp,
        start: impl FnOnce(File, u64) -> BoxedOp,
    ) -> Poll<io::Result<Done>> {
        let mut in_flight = match self.in_flight.take() {
            Some(op) if op.kind == kind => op,
            Some(mut other) => {
                match other.poll(cx) {
                    Poll::Pending => {
                        self.in_flight = Some(other);
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(done)) => self.apply(&done),
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                }
                InFlight::new(kind, start(self.file.clone(), self.cursor))
            }
            None => InFlight::new(kind, start(self.file.clone(), self.cursor)),
        };
        match in_flight.poll(cx) {
            Poll::Pending => {
                self.in_flight = Some(in_flight);
                Poll::Pending
            }
            Poll::Ready(result) => Poll::Ready(result),
        }
    }

    /// Finish whatever stream operation is in flight, keeping its effect.
    fn poll_settle(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(mut in_flight) = self.in_flight.take() else {
            return Poll::Ready(Ok(()));
        };
        match in_flight.poll(cx) {
            Poll::Pending => {
                self.in_flight = Some(in_flight);
                Poll::Pending
            }
            Poll::Ready(result) => Poll::Ready(result.map(|done| self.apply(&done))),
        }
    }

    /// The effect of a completed operation nobody is waiting for: a write
    /// or a seek moved the cursor; a read's bytes are dropped and the
    /// cursor stays, so the next read returns them again.
    fn apply(&mut self, done: &Done) {
        match *done {
            Done::Read(_) => {}
            Done::Write { end, .. } => self.cursor = end,
            Done::Seek(position) => self.cursor = position,
        }
    }

    /// The owned read: `buf` goes to the kernel and comes back.
    async fn read_owned(&self, offset: u64, buf: AlignedBuf) -> io::Result<(usize, AlignedBuf)> {
        if buf.is_empty() {
            return Ok((0, buf));
        }
        self.constraints.check(offset, buf.as_slice())?;
        let BufResult(result, CompioAlignedBuf(buf)) =
            self.file.read_at(CompioAlignedBuf(buf), offset).await;
        Ok((result?, buf))
    }

    /// The owned write, visible or durable: `buf` goes to the kernel and
    /// comes back.
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
        let mut file = &self.file;
        let BufResult(result, CompioAlignedBuf(buf)) =
            file.write_at(CompioAlignedBuf(buf), offset).await;
        let written = result?;
        if let WriteKind::Durable = kind {
            self.file.sync_data().await?;
        }
        Ok((written, buf))
    }

    /// The borrowed write: checked against the caller's slice, then copied
    /// into an aligned bounce buffer for the owned write.
    async fn write_borrowed(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        let mut bounce = AlignedBuf::zeroed(buf.len(), self.constraints.memory_alignment());
        bounce.as_mut_slice().copy_from_slice(buf);
        let (written, _) = self.write_owned(offset, bounce, WriteKind::Visible).await?;
        Ok(written)
    }

    /// The borrowed read: through an aligned bounce buffer, copied out.
    async fn read_borrowed(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        let bounce = AlignedBuf::zeroed(buf.len(), self.constraints.memory_alignment());
        let (read, bounce) = self.read_owned(offset, bounce).await?;
        buf[..read].copy_from_slice(&bounce.as_slice()[..read]);
        Ok(read)
    }
}

impl StorageFile for CompioStorageFile {
    fn sync_all(&self) -> impl Future<Output = io::Result<()>> + Send {
        SendWrapper::new(self.file.sync_all())
    }

    fn sync_data(&self) -> impl Future<Output = io::Result<()>> + Send {
        SendWrapper::new(self.file.sync_data())
    }

    fn constraints(&self) -> IoConstraints {
        self.constraints
    }

    fn is_direct_io(&self) -> bool {
        self.direct
    }

    fn read_at(
        &self,
        offset: u64,
        buf: &mut [u8],
    ) -> impl Future<Output = io::Result<usize>> + Send {
        tracing::trace!(offset, len = buf.len(), "read_at");
        SendWrapper::new(self.read_borrowed(offset, buf))
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> impl Future<Output = io::Result<usize>> + Send {
        tracing::trace!(offset, len = buf.len(), "write_at");
        SendWrapper::new(self.write_borrowed(offset, buf))
    }

    fn read_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> + Send {
        tracing::trace!(offset, len = buf.len(), "read_at_aligned");
        SendWrapper::new(self.read_owned(offset, buf))
    }

    fn write_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> + Send {
        tracing::trace!(offset, len = buf.len(), "write_at_aligned");
        SendWrapper::new(self.write_owned(offset, buf, WriteKind::Visible))
    }

    /// The default's write-then-`sync_data`, kept zero-copy: the default
    /// would route the owned buffer through the borrowed write's bounce.
    fn write_at_durable_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> + Send {
        tracing::trace!(offset, len = buf.len(), "write_at_durable_aligned");
        SendWrapper::new(self.write_owned(offset, buf, WriteKind::Durable))
    }

    fn size(&self) -> impl Future<Output = io::Result<u64>> + Send {
        SendWrapper::new(async move { Ok(self.file.metadata().await?.len()) })
    }

    fn set_len(&self, size: u64) -> impl Future<Output = io::Result<()>> + Send {
        SendWrapper::new(self.file.set_len(size))
    }
}

impl AsyncRead for CompioStorageFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !this.constraints.is_unconstrained() {
            return Poll::Ready(Err(stream_io_unsupported()));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let len = buf.len();
        let done = std::task::ready!(this.poll_stream(cx, StreamOp::Read, |file, cursor| {
            Box::pin(async move {
                let BufResult(result, data) = file.read_at(Vec::with_capacity(len), cursor).await;
                result.map(|_| Done::Read(data))
            })
        }))?;
        let Done::Read(data) = done else {
            return Poll::Ready(Err(unexpected_completion()));
        };
        // The operation was sized by the poll that started it; a later poll
        // with a smaller buffer takes what fits and leaves the rest unread.
        let read = data.len().min(buf.len());
        buf[..read].copy_from_slice(&data[..read]);
        this.cursor += read as u64;
        tracing::trace!(read, "stream read");
        Poll::Ready(Ok(read))
    }
}

impl AsyncWrite for CompioStorageFile {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !this.constraints.is_unconstrained() {
            return Poll::Ready(Err(stream_io_unsupported()));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let append = this.append;
        let done = std::task::ready!(this.poll_stream(cx, StreamOp::Write, |file, cursor| {
            let data = buf.to_vec();
            Box::pin(async move {
                let at = if append {
                    file.metadata().await?.len()
                } else {
                    cursor
                };
                let mut handle = &file;
                let BufResult(result, _) = handle.write_at(data, at).await;
                let written = result?;
                Ok(Done::Write {
                    written,
                    end: at + written as u64,
                })
            })
        }))?;
        let Done::Write { written, end } = done else {
            return Poll::Ready(Err(unexpected_completion()));
        };
        this.cursor = end;
        tracing::trace!(written, "stream write");
        Poll::Ready(Ok(written))
    }

    /// Stream writes complete before `poll_write` returns, so a flush only
    /// finishes a write whose caller stopped polling it.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_settle(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_settle(cx)
    }
}

impl AsyncSeek for CompioStorageFile {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let done = std::task::ready!(this.poll_stream(cx, StreamOp::Seek(pos), |file, cursor| {
            Box::pin(async move {
                let (base, delta) = match pos {
                    SeekFrom::Start(position) => return Ok(Done::Seek(position)),
                    SeekFrom::Current(delta) => (cursor, delta),
                    SeekFrom::End(delta) => (file.metadata().await?.len(), delta),
                };
                base.checked_add_signed(delta)
                    .map(Done::Seek)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "invalid seek to a negative or overflowing position",
                        )
                    })
            })
        }))?;
        let Done::Seek(position) = done else {
            return Poll::Ready(Err(unexpected_completion()));
        };
        this.cursor = position;
        Poll::Ready(Ok(position))
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenOptions, access_only, is_direct_io_unsupported};
    use std::io;

    /// Only the two codes that mean "this filesystem cannot do direct I/O"
    /// may be absorbed into a buffered fallback (as in the Tokio backend).
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
            libc::EROFS,
            libc::EIO,
        ] {
            assert!(!is_direct_io_unsupported(&io::Error::from_raw_os_error(
                code
            )));
        }
        assert!(!is_direct_io_unsupported(&io::Error::other("synthetic")));
    }

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
        assert!(access.is_read() && access.is_write() && access.is_append());
        assert!(!access.is_create() && !access.is_create_new() && !access.is_truncate());
    }
}
