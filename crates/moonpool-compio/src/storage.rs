//! Files on the compio runtime: [`CompioStorageProvider`] and
//! [`CompioStorageFile`].
//!
//! The open protocol (direct-I/O policy, the `Optional` upgrade by
//! descriptor, the `Required` refusal rules) follows `moonpool-core`'s Tokio
//! backend, over the shared helpers in [`moonpool_core::direct_io`] (the
//! alignment probe, error classification and constructors), where the
//! long-form reasoning lives.

use std::fmt;
use std::future::Future;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

use compio::BufResult;
use compio::buf::IoBuf;
use compio::fs::File;
use compio::io::{AsyncReadAt, AsyncWriteAt, AsyncWriteAtExt as _};
use compio_send_wrapper::SendWrapper;
use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};
use moonpool_core::direct_io::{
    DIRECT_IO_SUPPORTED, access_only, creation_needs_a_bootstrap, direct_io_constraints,
    is_direct_io_unsupported, undescribable_direct_io, unsupported_direct_io,
};
#[cfg(target_os = "linux")]
use moonpool_core::direct_io::{descriptor_path, descriptor_paths_available};
use moonpool_core::{
    AlignedBuf, DirectIo, IoConstraints, OpenOptions, StorageFile, StorageProvider,
    stream_io_unsupported,
};

use crate::buf::CompioAlignedBuf;

#[cfg(unix)]
mod append;
#[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
mod dsync;

#[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
use dsync::write_at_durable;

/// One durable positioned write: without `pwritev2(RWF_DSYNC)`, a write then
/// `sync_data`.
#[cfg(not(all(target_os = "linux", any(target_env = "gnu", target_env = "musl"))))]
async fn write_at_durable<T: IoBuf>(file: &File, offset: u64, buffer: T) -> BufResult<usize, T> {
    write_then_sync(file, offset, buffer).await
}

/// The portable durable write: a positioned write, then a data sync once the
/// write succeeded. The sync covers more than the write's range, which the
/// contract allows. The buffer comes back with the result.
async fn write_then_sync<T: IoBuf>(file: &File, offset: u64, buffer: T) -> BufResult<usize, T> {
    let mut handle = file;
    let BufResult(result, buffer) = handle.write_at(buffer, offset).await;
    let result = match result {
        Ok(written) => file.sync_data().await.map(|()| written),
        Err(error) => Err(error),
    };
    BufResult(result, buffer)
}

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
        tracing::trace!(path, "exists");
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
    let append_file = open_appender(&path, &opened, &options).await?;
    Ok(CompioStorageFile {
        file: opened.file,
        append_file,
        constraints: opened.constraints,
        direct: opened.direct,
        append: options.is_append(),
        cursor: 0,
        in_flight: None,
    })
}

/// The descriptor stream writes of an append-mode file go through: a second
/// open of the same file with `O_APPEND`, so the kernel places each write at
/// the end atomically. It carries the caller's access and no lifecycle flag
/// (the first open already created or truncated the file), and on Linux it
/// reopens the primary descriptor's own file through `/proc/self/fd`, immune
/// to a rename between the two opens. A direct-I/O file refuses stream I/O,
/// so it gets none.
#[cfg(unix)]
async fn open_appender(
    path: &str,
    opened: &OpenedFile,
    options: &OpenOptions,
) -> io::Result<Option<File>> {
    if !options.is_append() || opened.direct {
        return Ok(None);
    }
    let mut builder = compio_options(&access_only(options), false);
    builder.custom_flags(libc::O_APPEND);
    #[cfg(target_os = "linux")]
    if descriptor_paths_available() {
        return builder
            .open(descriptor_path(&opened.file, path))
            .await
            .map(Some);
    }
    builder.open(path).await.map(Some)
}

/// No `O_APPEND` through compio outside unix: stream writes emulate append
/// (see [`CompioStorageFile`]).
#[cfg(not(unix))]
async fn open_appender(
    _path: &str,
    _opened: &OpenedFile,
    _options: &OpenOptions,
) -> io::Result<Option<File>> {
    Ok(None)
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
// The open protocol: the Tokio backend's, over compio's open.
// ---------------------------------------------------------------------------

/// Build compio open options for `options`, optionally adding `O_DIRECT`.
///
/// compio has no append mode. Append implies write access here; stream
/// writes go through a second, `O_APPEND` descriptor (see
/// [`CompioStorageFile`]). The primary descriptor never carries `O_APPEND`,
/// so a positioned write always lands where it is told.
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
            upgrade_to_direct(buffered, path, options).await
        }
    }
}

/// The `Optional` upgrade: reopen the file the buffered open holds, by
/// descriptor, with `O_DIRECT` and no lifecycle flags. Only "no direct I/O
/// here" failures fall back to the buffered handle.
#[cfg(target_os = "linux")]
async fn upgrade_to_direct(
    buffered: File,
    path: &str,
    options: &OpenOptions,
) -> io::Result<OpenedFile> {
    if !descriptor_paths_available() {
        return Ok(OpenedFile::buffered(buffered));
    }
    let upgrade = descriptor_path(&buffered, path);
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
async fn upgrade_to_direct(
    buffered: File,
    _path: &str,
    _options: &OpenOptions,
) -> io::Result<OpenedFile> {
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
/// and its owned form) are one `pwritev2(RWF_DSYNC)` operation on Linux, as
/// in the Tokio backend: an `io_uring` write carrying `RWF_DSYNC` under the
/// `io_uring` driver, a `pwritev2` on the blocking pool under the polling
/// driver. A kernel that refuses it (`ENOSYS` or `EOPNOTSUPP`) switches the
/// whole process, once, to a write followed by `sync_data`, which is also
/// the only form outside Linux. The borrowed form copies through the aligned
/// bounce buffer into the same operation.
///
/// **Stream I/O** (on a buffered file only; a direct-I/O file refuses it
/// with [`stream_io_unsupported`]) is emulated over positioned operations
/// and a cursor kept here, since compio files have no cursor. Each
/// `poll_read` / `poll_write` runs one operation through an owned buffer
/// (one copy). A read returns once its operation completed. A write follows
/// Tokio's `File`: `poll_write` returning `Ready(n)` means the `n` bytes were
/// **accepted**, copied into an operation now in flight, not that they
/// reached the file. The next `poll_write` (or any other stream operation)
/// first waits for that operation, returning `Pending` without taking the
/// new buffer, and `poll_flush` / `poll_close` settle it. A failed write
/// surfaces on that next call. So a caller cancelled while waiting has
/// handed over nothing, and every accepted byte is written once settled. A
/// file opened with `append(true)` sends its stream writes through a second
/// descriptor opened with `O_APPEND`, as plain `write(2)` calls, so the kernel
/// appends each one atomically, even against another writer appending to the same file, and
/// the cursor moves to the file's size once the write completes. Positioned writes keep using the
/// primary descriptor, which has no `O_APPEND`. (Outside unix, where compio
/// cannot set `O_APPEND`, a stream write goes at the size read just before
/// it, which is not atomic against concurrent appenders.)
///
/// Every future this file returns, and the file itself while a stream
/// operation is in flight, is bound to the runtime thread that created it
/// (see the crate documentation).
pub struct CompioStorageFile {
    file: File,
    /// The `O_APPEND` descriptor stream writes use, on an append-mode
    /// buffered file under unix.
    append_file: Option<File>,
    constraints: IoConstraints,
    direct: bool,
    append: bool,
    /// The stream cursor: where the next stream read or write goes.
    cursor: u64,
    /// The stream operation still running: a read or seek whose `poll_*`
    /// returned `Pending`, or an accepted write.
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
    /// The whole accepted buffer was written; the cursor after it.
    Write { end: u64 },
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
    /// operation of another kind (an accepted write, or one whose `poll_*`
    /// caller went away) is finished first and its effect on the cursor kept.
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
            Done::Write { end } => self.cursor = end,
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
        let BufResult(result, CompioAlignedBuf(buf)) =
            self.write_buf(offset, CompioAlignedBuf(buf), kind).await;
        Ok((result?, buf))
    }

    /// One positioned write of an owned buffer, visible or durable.
    async fn write_buf<T: IoBuf>(
        &self,
        offset: u64,
        buf: T,
        kind: WriteKind,
    ) -> BufResult<usize, T> {
        match kind {
            WriteKind::Visible => {
                let mut file = &self.file;
                file.write_at(buf, offset).await
            }
            WriteKind::Durable => write_at_durable(&self.file, offset, buf).await,
        }
    }

    /// The borrowed write, visible or durable, checked against the caller's
    /// slice. An unconstrained file writes a plain copy; a direct-I/O file
    /// copies into an aligned bounce buffer for the owned write.
    async fn write_borrowed(&self, offset: u64, buf: &[u8], kind: WriteKind) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        if self.constraints.is_unconstrained() {
            let BufResult(result, _) = self.write_buf(offset, buf.to_vec(), kind).await;
            return result;
        }
        let mut bounce = AlignedBuf::zeroed(buf.len(), self.constraints.memory_alignment());
        bounce.as_mut_slice().copy_from_slice(buf);
        let (written, _) = self.write_owned(offset, bounce, kind).await?;
        Ok(written)
    }

    /// The borrowed read, copied out of a plain buffer on an unconstrained
    /// file, out of an aligned bounce buffer on a direct-I/O file.
    async fn read_borrowed(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.constraints.check(offset, buf)?;
        if self.constraints.is_unconstrained() {
            let BufResult(result, data) = self
                .file
                .read_at(Vec::with_capacity(buf.len()), offset)
                .await;
            result?;
            buf[..data.len()].copy_from_slice(&data);
            return Ok(data.len());
        }
        let bounce = AlignedBuf::zeroed(buf.len(), self.constraints.memory_alignment());
        let (read, bounce) = self.read_owned(offset, bounce).await?;
        buf[..read].copy_from_slice(&bounce.as_slice()[..read]);
        Ok(read)
    }
}

impl StorageFile for CompioStorageFile {
    fn sync_all(&self) -> impl Future<Output = io::Result<()>> + Send {
        tracing::trace!("sync_all");
        SendWrapper::new(self.file.sync_all())
    }

    fn sync_data(&self) -> impl Future<Output = io::Result<()>> + Send {
        tracing::trace!("sync_data");
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
        SendWrapper::new(self.write_borrowed(offset, buf, WriteKind::Visible))
    }

    /// One `RWF_DSYNC` write through the aligned bounce buffer, instead of
    /// the default's write then `sync_data`.
    fn write_at_durable(
        &self,
        offset: u64,
        buf: &[u8],
    ) -> impl Future<Output = io::Result<usize>> + Send {
        tracing::trace!(offset, len = buf.len(), "write_at_durable");
        SendWrapper::new(self.write_borrowed(offset, buf, WriteKind::Durable))
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

    /// One `RWF_DSYNC` write, kept zero-copy: the default would route the
    /// owned buffer through the borrowed write's bounce.
    fn write_at_durable_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> impl Future<Output = io::Result<(usize, AlignedBuf)>> + Send {
        tracing::trace!(offset, len = buf.len(), "write_at_durable_aligned");
        SendWrapper::new(self.write_owned(offset, buf, WriteKind::Durable))
    }

    fn size(&self) -> impl Future<Output = io::Result<u64>> + Send {
        tracing::trace!("size");
        SendWrapper::new(async move { Ok(self.file.metadata().await?.len()) })
    }

    fn set_len(&self, size: u64) -> impl Future<Output = io::Result<()>> + Send {
        tracing::trace!(size, "set_len");
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
    /// Settle the write in flight, then accept `buf`: copy it into a new
    /// write operation, start it, and return `Ready(buf.len())` without
    /// waiting for it (see the type documentation).
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
        std::task::ready!(this.poll_settle(cx))?;
        let len = buf.len();
        let op = stream_write(
            this.file.clone(),
            this.append_file.clone(),
            this.append,
            this.cursor,
            buf.to_vec(),
        );
        if !this.append {
            this.cursor += len as u64;
        }
        let mut in_flight = InFlight::new(StreamOp::Write, op);
        match in_flight.poll(cx) {
            Poll::Pending => this.in_flight = Some(in_flight),
            Poll::Ready(Ok(done)) => this.apply(&done),
            // Accepted, then failed at once: the error waits for the next
            // call, as it would had the write been slower.
            Poll::Ready(Err(error)) => {
                this.in_flight = Some(InFlight::new(
                    StreamOp::Write,
                    Box::pin(std::future::ready(Err(error))),
                ));
            }
        }
        tracing::trace!(len, "stream write accepted");
        Poll::Ready(Ok(len))
    }

    /// Wait for the accepted write, surfacing its error.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_settle(cx)
    }

    /// Wait for the accepted write, surfacing its error.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_settle(cx)
    }
}

/// The operation behind an accepted stream write: all of `data`, at the
/// cursor, or at the end of an append-mode file.
fn stream_write(
    file: File,
    appender: Option<File>,
    append: bool,
    cursor: u64,
    data: Vec<u8>,
) -> BoxedOp {
    Box::pin(async move {
        let len = data.len() as u64;
        #[cfg(unix)]
        if let Some(appender) = appender {
            // A plain `write(2)` on the `O_APPEND` descriptor goes to the end
            // of the file, atomically, on every unix (`pwrite` would not on
            // macOS, which honours its offset).
            append::append_all(&appender, data).await?;
            return Ok(Done::Write {
                end: appender.metadata().await?.len(),
            });
        }
        #[cfg(not(unix))]
        let _ = appender;
        let at = if append {
            file.metadata().await?.len()
        } else {
            cursor
        };
        let mut handle = &file;
        let BufResult(result, _) = handle.write_all_at(data, at).await;
        result?;
        Ok(Done::Write { end: at + len })
    })
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
    use super::OpenOptions;
    use std::io;

    /// Append mode is the kernel's: stream writes go through a descriptor
    /// with `O_APPEND`, positioned writes through one without it.
    #[cfg(unix)]
    #[compio::test]
    async fn only_the_append_descriptor_carries_o_append() -> io::Result<()> {
        use std::os::fd::{AsFd as _, AsRawFd as _};

        fn status_flags(file: &compio::fs::File) -> libc::c_int {
            // SAFETY: `F_GETFL` reads the open file's status flags; the
            // descriptor is open for the duration of the call.
            unsafe { libc::fcntl(file.as_fd().as_raw_fd(), libc::F_GETFL) }
        }

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("log");
        let path = path.to_str().expect("UTF-8 path").to_owned();
        let file = super::open(path.clone(), OpenOptions::create_write().append(true)).await?;
        let appender = file.append_file.as_ref().expect("an append descriptor");
        assert_ne!(status_flags(appender) & libc::O_APPEND, 0);
        assert_eq!(status_flags(&file.file) & libc::O_APPEND, 0);

        let plain = super::open(path, OpenOptions::read_write()).await?;
        assert!(
            plain.append_file.is_none(),
            "no append, no second descriptor"
        );
        Ok(())
    }
}
