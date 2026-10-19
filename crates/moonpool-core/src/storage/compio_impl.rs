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

use super::{
    AlignedBuf, DirectIo, IoConstraints, LocalStorageFile, LocalStorageProvider, OpenOptions,
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
/// This backend does no direct I/O yet. [`DirectIo::Optional`] opens a
/// buffered file, which the policy allows, and [`DirectIo::Required`] is
/// refused with [`io::ErrorKind::Unsupported`]. Every file it opens has
/// [`IoConstraints::NONE`].
///
/// ## Append
///
/// The descriptor is opened without `O_APPEND`, because a positioned write
/// on an `O_APPEND` descriptor ignores its offset on Linux. Append mode is
/// applied by the stream instead: each stream write of an append file goes to
/// the end of the file as it is when the write starts. Positioned writes go
/// exactly where they are told, as the contract says. Unlike `O_APPEND`, this
/// is not atomic against other writers of the same file.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompioStorageProvider;

impl CompioStorageProvider {
    /// Create a compio storage provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl LocalStorageProvider for CompioStorageProvider {
    type File = CompioStorageFile;

    async fn open(&self, path: &str, options: OpenOptions) -> io::Result<Self::File> {
        options.validate()?;
        if options.requested_direct_io() == DirectIo::Required {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("direct I/O is not supported by the compio backend for '{path}'"),
            ));
        }
        let file = compio::fs::OpenOptions::new()
            .read(options.is_read())
            // Append is applied by the stream, on a descriptor without
            // O_APPEND: see the provider docs.
            .write(options.is_write() || options.is_append())
            .create(options.is_create())
            .create_new(options.is_create_new())
            .truncate(options.is_truncate())
            .open(path)
            .await?;
        Ok(CompioStorageFile {
            file,
            append: options.is_append(),
            cursor: 0,
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
/// borrowed forms copy through a buffer of their own, because a completion
/// operation must own its memory until the kernel is done with it.
///
/// The stream half ([`AsyncRead`], [`AsyncWrite`], [`AsyncSeek`]) keeps its
/// own cursor and runs one positioned operation at a time at it. A started
/// operation stays in the file: polling again resumes it, so a dropped read
/// or write future loses nothing. A stream write that is pending when a
/// stream read or seek is polled is finished first, and its bytes count.
pub struct CompioStorageFile {
    file: compio::fs::File,
    /// Stream writes go to the end of the file (see the provider docs).
    append: bool,
    /// The stream cursor.
    cursor: u64,
    /// The stream operation in flight, if any.
    pending: Option<Pending>,
}

impl std::fmt::Debug for CompioStorageFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompioStorageFile")
            .field("file", &self.file)
            .field("append", &self.append)
            .field("cursor", &self.cursor)
            .field("pending", &self.pending.as_ref().map(Pending::kind))
            .finish()
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

/// A stream operation in flight. Each owns a clone of the file handle and its
/// buffer, so it outlives any one poll.
enum Pending {
    /// A read at `start`, into an owned buffer.
    Read {
        start: u64,
        op: LocalFuture<BufResult<usize, Vec<u8>>>,
    },
    /// A write; resolves to the cursor after it and the bytes written.
    Write(LocalFuture<io::Result<(u64, usize)>>),
    /// A seek that needs the file's size; resolves to the new cursor.
    Seek(LocalFuture<io::Result<u64>>),
}

/// A finished stream operation.
enum Done {
    Read {
        start: u64,
        result: io::Result<usize>,
        buf: Vec<u8>,
    },
    Write(io::Result<(u64, usize)>),
    Seek(io::Result<u64>),
}

impl Pending {
    fn kind(&self) -> Kind {
        match self {
            Self::Read { .. } => Kind::Read,
            Self::Write(_) => Kind::Write,
            Self::Seek(_) => Kind::Seek,
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Done> {
        match self {
            Self::Read { start, op } => {
                let BufResult(result, buf) = ready!(op.as_mut().poll(cx));
                Poll::Ready(Done::Read {
                    start: *start,
                    result,
                    buf,
                })
            }
            Self::Write(op) => op.as_mut().poll(cx).map(Done::Write),
            Self::Seek(op) => op.as_mut().poll(cx).map(Done::Seek),
        }
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
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.kind() != want)
        {
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
            Done::Write(Ok((cursor, _))) | Done::Seek(Ok(cursor)) => self.cursor = cursor,
            Done::Read { .. } | Done::Write(Err(_)) | Done::Seek(Err(_)) => {}
        }
    }

    fn start_read(&self, len: usize) -> Pending {
        let file = self.file.clone();
        let start = self.cursor;
        Pending::Read {
            start,
            op: Box::pin(async move { file.read_at(Vec::with_capacity(len), start).await }),
        }
    }

    fn start_write(&self, bytes: Vec<u8>) -> Pending {
        let file = self.file.clone();
        let cursor = self.cursor;
        let append = self.append;
        Pending::Write(Box::pin(async move {
            let at = if append {
                file.metadata().await?.len()
            } else {
                cursor
            };
            let BufResult(result, _) = (&file).write_at(bytes, at).await;
            let written = result?;
            Ok((at + written as u64, written))
        }))
    }

    fn start_seek_from_end(&self, offset: i64) -> Pending {
        let file = self.file.clone();
        Pending::Seek(Box::pin(async move {
            let size = file.metadata().await?.len();
            size.checked_add_signed(offset).ok_or_else(invalid_seek)
        }))
    }

    /// The owned write: `buf` goes to the kernel and comes back.
    async fn write_owned(&self, offset: u64, buf: AlignedBuf) -> io::Result<(usize, AlignedBuf)> {
        if buf.is_empty() {
            return Ok((0, buf));
        }
        IoConstraints::NONE.check(offset, buf.as_slice())?;
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
        IoConstraints::NONE
    }

    fn is_direct_io(&self) -> bool {
        false
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let BufResult(result, bounce) = self
            .file
            .read_at(Vec::with_capacity(buf.len()), offset)
            .await;
        let read = result?;
        buf[..read].copy_from_slice(&bounce[..read]);
        Ok(read)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let BufResult(result, _) = (&self.file).write_at(buf.to_vec(), offset).await;
        result
    }

    async fn read_at_aligned(
        &self,
        offset: u64,
        buf: AlignedBuf,
    ) -> io::Result<(usize, AlignedBuf)> {
        if buf.is_empty() {
            return Ok((0, buf));
        }
        let BufResult(result, buf) = self.file.read_at(buf, offset).await;
        Ok((result?, buf))
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
}

impl AsyncRead for CompioStorageFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            ready!(this.poll_idle(cx));
            return Poll::Ready(Ok(0));
        }
        let len = buf.len();
        match ready!(this.poll_op(cx, Kind::Read, |file| file.start_read(len))) {
            Done::Read {
                start,
                result,
                buf: read,
            } => {
                let read_len = result?;
                // A resumed read may hand a shorter buffer than it started
                // with: the cursor moves by what the caller received.
                let copied = read_len.min(buf.len());
                buf[..copied].copy_from_slice(&read[..copied]);
                this.cursor = start + copied as u64;
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
            Done::Read { .. } | Done::Seek(_) => unreachable!("poll_op returns a write"),
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
        match pos {
            SeekFrom::Start(position) => {
                ready!(this.poll_idle(cx));
                this.cursor = position;
                Poll::Ready(Ok(position))
            }
            SeekFrom::Current(offset) => {
                ready!(this.poll_idle(cx));
                let position = this
                    .cursor
                    .checked_add_signed(offset)
                    .ok_or_else(invalid_seek)?;
                this.cursor = position;
                Poll::Ready(Ok(position))
            }
            SeekFrom::End(offset) => {
                match ready!(this.poll_op(cx, Kind::Seek, |file| file.start_seek_from_end(offset)))
                {
                    Done::Seek(result) => {
                        let position = result?;
                        this.cursor = position;
                        Poll::Ready(Ok(position))
                    }
                    Done::Read { .. } | Done::Write(_) => unreachable!("poll_op returns a seek"),
                }
            }
        }
    }
}
