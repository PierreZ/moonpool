//! Compio [`LocalStorageProvider`] / [`LocalStorageFile`] over the real
//! filesystem.
//!
//! Every positioned transfer, sync and size query is one completion-based
//! operation on compio's driver: `io_uring` on Linux (polling where the kernel
//! refuses `io_uring`), IOCP on Windows. Nothing goes through a blocking
//! thread pool except listing a directory, which compio has no operation
//! for.

use std::future::Future;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use compio::buf::BufResult;
use compio::io::{AsyncReadAt, AsyncWriteAt};
use futures::io::{AsyncRead, AsyncSeek, AsyncWrite};

use super::native::{self, NativeFs, OpenedFile};
use super::{
    AlignedBuf, IoConstraints, LocalStorageFile, LocalStorageProvider, OpenOptions,
    stream_io_unsupported,
};

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
///
/// ## Append
///
/// On Linux an append file's stream descriptor carries `O_APPEND`, so each
/// stream write lands at the end of the file atomically, as with any other
/// writer. Positioned I/O runs on a second descriptor of the same file
/// without it, because a positioned write on an `O_APPEND` descriptor ignores
/// its offset on Linux. Elsewhere a positioned write honours its offset even
/// under `O_APPEND`, which is no help to the stream, so there the stream
/// itself writes at the end of the file as it is when the write starts: not
/// atomic against other writers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompioStorageProvider;

impl CompioStorageProvider {
    /// Create a compio storage provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Whether the kernel appends a positioned write to an `O_APPEND`
/// descriptor, so the stream can append atomically with `O_APPEND`.
const NATIVE_APPEND: bool = cfg!(target_os = "linux");

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
            .write(options.is_write() || options.is_append())
            .create(options.is_create())
            .create_new(options.is_create_new())
            .truncate(options.is_truncate());
        #[cfg(unix)]
        {
            let mut flags = 0;
            if options.is_append() && NATIVE_APPEND {
                flags |= libc::O_APPEND;
            }
            #[cfg(target_os = "linux")]
            if direct {
                flags |= libc::O_DIRECT;
            }
            if flags != 0 {
                builder.custom_flags(flags);
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = direct;
        builder.open(path).await
    }

    async fn truncate(file: &compio::fs::File) -> io::Result<()> {
        file.set_len(0).await
    }
}

/// How stream writes of a file reach its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Append {
    /// Not an append file: stream writes go to the cursor.
    No,
    /// The stream descriptor has `O_APPEND`: the kernel appends.
    Native,
    /// The stream writes at the size it reads first (see the provider docs).
    Emulated,
}

impl LocalStorageProvider for CompioStorageProvider {
    type File = CompioStorageFile;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        options.validate()?;
        let opened = native::open_file(&CompioFs, path, &options).await?;
        let append = match (options.is_append(), NATIVE_APPEND) {
            (false, _) => Append::No,
            (true, true) => Append::Native,
            (true, false) => Append::Emulated,
        };
        let file = if append == Append::Native {
            // A separate open without O_APPEND, of the file this descriptor
            // holds: see the provider docs.
            let positioned = OpenOptions::new().read(options.is_read()).write(true);
            native::reopen_same_file(&CompioFs, &opened.file, path, &positioned, opened.direct)
                .await?
        } else {
            opened.file.clone()
        };
        let OpenedFile {
            file: stream,
            constraints,
            direct,
        } = opened;
        Ok(CompioStorageFile {
            file,
            stream,
            append,
            constraints,
            direct,
            cursor: Some(0),
            pending: None,
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
/// Positioned I/O ([`read_at`](LocalStorageFile::read_at) and its siblings)
/// takes `&self` and never touches the stream cursor. The owned forms
/// (`*_aligned`) hand the caller's [`AlignedBuf`] to the kernel itself; the
/// borrowed forms copy through an aligned buffer of their own, because a
/// completion operation must own its memory until the kernel is done with it.
///
/// The stream half ([`AsyncRead`], [`AsyncWrite`], [`AsyncSeek`]) keeps its
/// own cursor and runs one operation at a time. A started operation stays in
/// the file: polling again resumes it, so a dropped read or write future
/// loses nothing. A stream write that is pending when a stream read or seek
/// is polled is finished first, and its bytes count. A file with alignment
/// constraints (a direct-I/O file) refuses stream reads and writes with
/// [`stream_io_unsupported`], as the contract requires.
pub struct CompioStorageFile {
    /// Positioned I/O, syncs and size queries.
    file: compio::fs::File,
    /// Stream writes. The same descriptor as `file`, except for a
    /// [`Append::Native`] file, where this one has `O_APPEND`.
    stream: compio::fs::File,
    append: Append,
    /// What this open requires of callers (see [`IoConstraints`]).
    constraints: IoConstraints,
    /// Whether the open got uncached I/O.
    direct: bool,
    /// The stream cursor. `None` after a native append: the end of the file,
    /// whose position is read when something needs it.
    cursor: Option<u64>,
    /// The stream operation in flight, if any.
    pending: Option<Pending>,
}

impl std::fmt::Debug for CompioStorageFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompioStorageFile")
            .field("file", &self.file)
            .field("append", &self.append)
            .field("constraints", &self.constraints)
            .field("direct", &self.direct)
            .field("cursor", &self.cursor)
            .field("pending", &self.pending.as_ref().map(Pending::kind))
            .finish_non_exhaustive()
    }
}

/// The kind of a stream operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Seek,
}

type LocalFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// A stream read's outcome: where it started, how much it read, the bytes.
type ReadOutcome = io::Result<(u64, usize, Vec<u8>)>;

/// A stream write's outcome: the cursor after it, and the bytes written.
type WriteOutcome = io::Result<(Option<u64>, usize)>;

/// A stream operation in flight. Each owns clones of the file handles and
/// its buffer, so it outlives any one poll.
enum Pending {
    Read(LocalFuture<ReadOutcome>),
    Write(LocalFuture<WriteOutcome>),
    /// A seek that needs the file's size; resolves to the new cursor.
    Seek(LocalFuture<io::Result<u64>>),
}

/// A finished stream operation.
enum Done {
    Read(ReadOutcome),
    Write(WriteOutcome),
    Seek(io::Result<u64>),
}

impl Pending {
    fn kind(&self) -> Kind {
        match self {
            Self::Read(_) => Kind::Read,
            Self::Write(_) => Kind::Write,
            Self::Seek(_) => Kind::Seek,
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Done> {
        match self {
            Self::Read(op) => op.as_mut().poll(cx).map(Done::Read),
            Self::Write(op) => op.as_mut().poll(cx).map(Done::Write),
            Self::Seek(op) => op.as_mut().poll(cx).map(Done::Seek),
        }
    }
}

/// The stream position `cursor` names: itself, or the current end of the
/// file.
async fn resolve(file: &compio::fs::File, cursor: Option<u64>) -> io::Result<u64> {
    match cursor {
        Some(position) => Ok(position),
        None => Ok(file.metadata().await?.len()),
    }
}

impl CompioStorageFile {
    /// Poll the stream operation of kind `want`: an operation of another kind
    /// still in flight is finished and applied first, then `start` begins
    /// one unless it is already running.
    fn poll_op(
        &mut self,
        cx: &mut Context<'_>,
        want: Kind,
        start: impl FnOnce(&Self) -> Pending,
    ) -> Poll<Done> {
        if !self.pending_is(want) {
            ready!(self.poll_idle(cx));
        }
        let mut pending = match self.pending.take() {
            Some(pending) => pending,
            None => start(self),
        };
        match pending.poll(cx) {
            Poll::Ready(done) => Poll::Ready(done),
            Poll::Pending => {
                self.pending = Some(pending);
                Poll::Pending
            }
        }
    }

    fn pending_is(&self, kind: Kind) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.kind() == kind)
    }

    /// Finish any stream operation in flight, applying its effect on the
    /// cursor. Its result is dropped: whoever started it stopped polling.
    fn poll_idle(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(pending) = self.pending.as_mut() {
            let done = ready!(pending.poll(cx));
            self.pending = None;
            self.apply(&done);
        }
        Poll::Ready(())
    }

    /// The cursor effect of a finished operation. A read moves nothing: its
    /// bytes were never handed to anyone.
    fn apply(&mut self, done: &Done) {
        match *done {
            Done::Write(Ok((cursor, _))) => self.cursor = cursor,
            Done::Seek(Ok(cursor)) => self.cursor = Some(cursor),
            Done::Read(_) | Done::Write(Err(_)) | Done::Seek(Err(_)) => {}
        }
    }

    /// Stream I/O cannot honour an alignment: refused on a constrained file.
    fn check_stream(&self) -> io::Result<()> {
        if self.constraints.is_unconstrained() {
            Ok(())
        } else {
            Err(stream_io_unsupported())
        }
    }

    fn start_read(&self, len: usize) -> Pending {
        let file = self.file.clone();
        let cursor = self.cursor;
        Pending::Read(Box::pin(async move {
            let start = resolve(&file, cursor).await?;
            let BufResult(result, buf) = file.read_at(Vec::with_capacity(len), start).await;
            Ok((start, result?, buf))
        }))
    }

    fn start_write(&self, bytes: Vec<u8>) -> Pending {
        let file = self.file.clone();
        let stream = self.stream.clone();
        let cursor = self.cursor;
        let append = self.append;
        Pending::Write(Box::pin(async move {
            if append == Append::Native {
                // O_APPEND: the kernel ignores the offset and appends.
                let BufResult(result, _) = (&stream).write_at(bytes, 0).await;
                return Ok((None, result?));
            }
            let at = if append == Append::Emulated {
                file.metadata().await?.len()
            } else {
                resolve(&file, cursor).await?
            };
            let BufResult(result, _) = (&stream).write_at(bytes, at).await;
            let written = result?;
            Ok((Some(at + written as u64), written))
        }))
    }

    fn start_seek_from_end(&self, offset: i64) -> Pending {
        let file = self.file.clone();
        Pending::Seek(Box::pin(async move {
            let size = file.metadata().await?.len();
            size.checked_add_signed(offset).ok_or_else(invalid_seek)
        }))
    }

    /// Seek relative to the end of the file, which needs its size.
    fn poll_seek_from_end(&mut self, cx: &mut Context<'_>, offset: i64) -> Poll<io::Result<u64>> {
        match ready!(self.poll_op(cx, Kind::Seek, |file| file.start_seek_from_end(offset))) {
            Done::Seek(result) => {
                let position = result?;
                self.cursor = Some(position);
                Poll::Ready(Ok(position))
            }
            Done::Read(_) | Done::Write(_) => unreachable!("poll_op returns a seek"),
        }
    }

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

fn invalid_seek() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid seek to a negative or overflowing position",
    )
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
        // A short metadata call on the descriptor: run on this thread.
        if !native::fallocate(&self.file, len)? && self.size().await? < len {
            self.file.set_len(len).await?;
        }
        Ok(())
    }

    async fn drop_cache(&self) -> io::Result<()> {
        // A short advice call on the descriptor: run on this thread.
        if self.direct {
            return Ok(());
        }
        native::drop_cache(&self.file)
    }
}

impl AsyncRead for CompioStorageFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.check_stream()?;
        if buf.is_empty() {
            ready!(this.poll_idle(cx));
            return Poll::Ready(Ok(0));
        }
        let len = buf.len();
        match ready!(this.poll_op(cx, Kind::Read, |file| file.start_read(len))) {
            Done::Read(result) => {
                let (start, read_len, read) = result?;
                // A resumed read may hand a shorter buffer than it started
                // with: the cursor moves by what the caller received.
                let copied = read_len.min(buf.len());
                buf[..copied].copy_from_slice(&read[..copied]);
                this.cursor = Some(start + copied as u64);
                Poll::Ready(Ok(copied))
            }
            Done::Write(_) | Done::Seek(_) => unreachable!("poll_op returns a read"),
        }
    }
}

impl AsyncWrite for CompioStorageFile {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.check_stream()?;
        if buf.is_empty() {
            ready!(this.poll_idle(cx));
            return Poll::Ready(Ok(0));
        }
        match ready!(this.poll_op(cx, Kind::Write, |file| file.start_write(buf.to_vec()))) {
            Done::Write(result) => {
                let (cursor, written) = result?;
                this.cursor = cursor;
                Poll::Ready(Ok(written))
            }
            Done::Read(_) | Done::Seek(_) => unreachable!("poll_op returns a write"),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(pending) = this.pending.as_mut() {
            let done = ready!(pending.poll(cx));
            this.pending = None;
            // A write nobody collected still reports its error here.
            if let Done::Write(Err(error)) = done {
                return Poll::Ready(Err(error));
            }
            this.apply(&done);
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl AsyncSeek for CompioStorageFile {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        // A seek already in flight is this call, resumed: finishing it as
        // idle work would apply its offset twice.
        if this.pending_is(Kind::Seek)
            && let SeekFrom::End(offset) | SeekFrom::Current(offset) = pos
        {
            return this.poll_seek_from_end(cx, offset);
        }
        ready!(this.poll_idle(cx));
        match (pos, this.cursor) {
            (SeekFrom::Start(position), _) => {
                this.cursor = Some(position);
                Poll::Ready(Ok(position))
            }
            (SeekFrom::Current(offset), Some(cursor)) => {
                let position = cursor.checked_add_signed(offset).ok_or_else(invalid_seek)?;
                this.cursor = Some(position);
                Poll::Ready(Ok(position))
            }
            // After a native append the cursor is the end of the file.
            (SeekFrom::Current(offset), None) | (SeekFrom::End(offset), _) => {
                this.poll_seek_from_end(cx, offset)
            }
        }
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
    /// file's descriptor really has `O_DIRECT`, and an append file's stream
    /// descriptor has `O_APPEND` while its positioned one does not.
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
                drop(file);

                let log = storage
                    .open(&path, OpenOptions::new().read(true).append(true))
                    .await
                    .expect("open append");
                assert_ne!(status_flags(&log.stream) & libc::O_APPEND, 0, "{driver:?}");
                assert_eq!(status_flags(&log.file) & libc::O_APPEND, 0, "{driver:?}");
            });
        }
    }
}
