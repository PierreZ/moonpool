//! TCP on the compio runtime: [`CompioNetworkProvider`],
//! [`CompioTcpListener`] and [`CompioTcpStream`].

use std::fmt;
use std::future::Future;
use std::io;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use compio::BufResult;
use compio::buf::IntoInner as _;
use compio::io::{AsyncRead as _, AsyncWrite as _};
use compio::net::{TcpListener, TcpStream};
use compio::runtime::{JoinError, JoinHandle};
use compio_send_wrapper::SendWrapper;
use futures::io::{AsyncRead, AsyncWrite};
use moonpool_core::{NetworkProvider, TcpListenerTrait};

/// Network provider over `compio::net`.
///
/// Addresses are `host:port` strings, resolved the way compio resolves them
/// (a name goes through the operating system's lookup on a blocking thread).
/// Every connected and accepted stream has Nagle's algorithm disabled
/// (`TCP_NODELAY`), best-effort, for the same reasons as
/// `TokioNetworkProvider`: request/reply protocols write a small frame and
/// wait for its answer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompioNetworkProvider;

impl CompioNetworkProvider {
    /// Create a compio network provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[tracing::instrument(skip_all, fields(addr = %addr))]
async fn bind(addr: String) -> io::Result<CompioTcpListener> {
    let inner = TcpListener::bind(addr.as_str()).await?;
    Ok(CompioTcpListener { inner })
}

#[tracing::instrument(skip_all, fields(addr = %addr))]
async fn connect(addr: String) -> io::Result<CompioTcpStream> {
    let stream = TcpStream::connect(addr.as_str()).await?;
    Ok(CompioTcpStream::new(stream))
}

impl NetworkProvider for CompioNetworkProvider {
    type TcpStream = CompioTcpStream;
    type TcpListener = CompioTcpListener;

    fn bind(&self, addr: &str) -> impl Future<Output = io::Result<Self::TcpListener>> + Send {
        SendWrapper::new(bind(addr.to_owned()))
    }

    fn connect(&self, addr: &str) -> impl Future<Output = io::Result<Self::TcpStream>> + Send {
        SendWrapper::new(connect(addr.to_owned()))
    }
}

/// The listener [`CompioNetworkProvider::bind`](NetworkProvider::bind)
/// returns: a compio `TcpListener` whose accepted streams have `TCP_NODELAY`
/// set.
#[derive(Debug)]
pub struct CompioTcpListener {
    inner: TcpListener,
}

#[tracing::instrument(skip_all)]
async fn accept(listener: &TcpListener) -> io::Result<(CompioTcpStream, String)> {
    let (stream, peer) = listener.accept().await?;
    Ok((CompioTcpStream::new(stream), peer.to_string()))
}

impl TcpListenerTrait for CompioTcpListener {
    type TcpStream = CompioTcpStream;

    fn accept(&self) -> impl Future<Output = io::Result<(Self::TcpStream, String)>> + Send {
        SendWrapper::new(accept(&self.inner))
    }

    fn local_addr(&self) -> io::Result<String> {
        Ok(self.inner.local_addr()?.to_string())
    }
}

/// Capacity the read-ahead buffer starts with.
const READ_BUFFER_INITIAL: usize = 8 * 1024;

/// Capacity the read-ahead buffer grows to at most, when callers read with
/// larger buffers than it.
const READ_BUFFER_MAX: usize = 64 * 1024;

/// Most bytes one `poll_write` accepts: a larger write is a partial write,
/// which `write_all` loops over, so the owned write buffer stays bounded.
const WRITE_CHUNK_MAX: usize = 64 * 1024;

/// An in-flight socket read: it owns a clone of the stream (a shared file
/// descriptor) and its buffer, which it hands back on completion.
type ReadOp = Pin<Box<dyn Future<Output = BufResult<usize, Vec<u8>>>>>;

/// An in-flight write-side shutdown.
type ShutdownOp = Pin<Box<dyn Future<Output = io::Result<()>>>>;

/// A TCP stream implementing `futures::io::{AsyncRead, AsyncWrite}`.
///
/// compio's socket operations are completion-based and take owned buffers,
/// so the stream holds one operation per direction across polls, each on its
/// own handle to the socket: a read and a write can be in flight at the same
/// time (a stream split into halves, as moonpool-rpc does, keeps both
/// directions busy).
///
/// **Writes go through without a flush**, like the Tokio provider's stream.
/// A `poll_write` that returns `Ready(Ok(n))` has copied `n` bytes (at most
/// 64 KiB per call) and handed them to a runtime task that sends them all,
/// short sends included, whether or not the stream is polled again;
/// `poll_flush` only waits for that send to finish. At most one send is
/// outstanding, because independent sends on one socket are not ordered:
/// the next `poll_write` first waits for the previous one, returning
/// `Pending` meanwhile, which is the stream's backpressure. A send error
/// surfaces on the next `poll_write`, `poll_flush` or `poll_close`, as the
/// next syscall reports it on a Tokio stream. After `poll_close`, writes fail
/// with `BrokenPipe`. Dropping the stream leaves an outstanding send running,
/// so accepted bytes are not lost.
///
/// Each accepted write is one compio task, not an operation held in the
/// stream, because compio operations are lazy: nothing reaches the kernel
/// until the operation is polled, and a short send needs another poll to
/// resubmit its tail. A caller that writes a request and then waits on the
/// read side never polls the write side again, so a held operation would
/// leave the request (or its tail) unsent. A read is different: the caller
/// keeps polling the read it waits on, so `poll_read` polls its operation
/// once to submit it before returning `Pending`.
///
/// **Reads go through a read-ahead buffer** of 8 KiB, grown up to 64 KiB
/// when callers read with larger buffers: bytes received beyond what one
/// `poll_read` asked for are served by the next one. Each direction costs
/// one copy, between the caller's buffer and the stream's owned one.
///
/// The stream is bound to the runtime thread that created it: polling or
/// dropping it on another thread panics. See the crate documentation.
pub struct CompioTcpStream {
    inner: SendWrapper<Inner>,
}

/// Both directions of a [`CompioTcpStream`], on the thread that owns them.
struct Inner {
    reader: Reader,
    writer: Writer,
}

impl CompioTcpStream {
    fn new(stream: TcpStream) -> Self {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "TCP_NODELAY not set; the connection is used as it is");
        }
        let reader = Reader {
            stream: stream.clone(),
            buffer: Vec::with_capacity(READ_BUFFER_INITIAL),
            consumed: 0,
            op: None,
        };
        let writer = Writer {
            stream,
            state: WriteState::Idle(Vec::new()),
        };
        Self {
            inner: SendWrapper::new(Inner { reader, writer }),
        }
    }

    /// Both directions, on the thread that owns them.
    fn inner(&mut self) -> &mut Inner {
        self.inner
            .get_mut()
            .expect("a compio TCP stream was used off the runtime thread that created it")
    }
}

impl fmt::Debug for CompioTcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompioTcpStream").finish_non_exhaustive()
    }
}

/// The read direction: one in-flight receive into an owned buffer, and the
/// bytes received but not yet handed out.
struct Reader {
    stream: TcpStream,
    /// Received bytes are `buffer[consumed..]`. The buffer is moved into
    /// `op` while a receive is in flight and comes back with its result.
    buffer: Vec<u8>,
    consumed: usize,
    op: Option<ReadOp>,
}

impl Reader {
    fn poll_read(&mut self, cx: &mut Context<'_>, out: &mut [u8]) -> Poll<io::Result<usize>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let op = match self.op.take() {
            Some(op) => op,
            None if self.consumed < self.buffer.len() => {
                return Poll::Ready(Ok(self.copy_out(out)));
            }
            None => self.submit(out.len()),
        };
        let op = self.op.insert(op);
        let BufResult(result, buffer) = ready!(op.as_mut().poll(cx));
        self.op = None;
        self.buffer = buffer;
        match result {
            // The buffer's length is what was received; zero is end of stream.
            Ok(_) => Poll::Ready(Ok(self.copy_out(out))),
            Err(error) => {
                self.buffer.clear();
                Poll::Ready(Err(error))
            }
        }
    }

    /// Start a receive into the (emptied) buffer, grown towards `wanted`.
    fn submit(&mut self, wanted: usize) -> ReadOp {
        let mut buffer = mem::take(&mut self.buffer);
        buffer.clear();
        self.consumed = 0;
        let wanted = wanted.min(READ_BUFFER_MAX);
        if wanted > buffer.capacity() {
            buffer.reserve_exact(wanted);
        }
        let mut stream = self.stream.clone();
        Box::pin(async move { stream.read(buffer).await })
    }

    /// Hand out as many buffered bytes as fit in `out`.
    fn copy_out(&mut self, out: &mut [u8]) -> usize {
        let available = &self.buffer[self.consumed..];
        let copied = available.len().min(out.len());
        out[..copied].copy_from_slice(&available[..copied]);
        self.consumed += copied;
        copied
    }
}

/// The write direction's state.
enum WriteState {
    /// No send outstanding; the reusable write buffer, empty.
    Idle(Vec<u8>),
    /// One send outstanding, running as its own task.
    Sending(JoinHandle<(io::Result<()>, Vec<u8>)>),
    /// The write-side shutdown in flight.
    Closing(ShutdownOp),
    /// Shut down: further writes fail.
    Closed,
}

/// The write direction: at most one operation outstanding.
struct Writer {
    stream: TcpStream,
    state: WriteState,
}

impl Writer {
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        ready!(self.poll_drain(cx))?;
        let WriteState::Idle(buffer) = &mut self.state else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "write on a compio TCP stream after close",
            )));
        };
        let accepted = buf.len().min(WRITE_CHUNK_MAX);
        let mut buffer = mem::take(buffer);
        buffer.extend_from_slice(&buf[..accepted]);
        let stream = self.stream.clone();
        self.state = WriteState::Sending(compio::runtime::spawn(send_all(stream, buffer)));
        Poll::Ready(Ok(accepted))
    }

    /// Wait for the outstanding send. Leaves the state `Idle`, `Closing` or
    /// `Closed`.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let WriteState::Sending(send) = &mut self.state else {
            return Poll::Ready(Ok(()));
        };
        let (result, mut buffer) = match ready!(Pin::new(send).poll(cx)) {
            Ok(outcome) => outcome,
            Err(JoinError::Cancelled) => (
                Err(io::Error::other("a compio TCP send task was cancelled")),
                Vec::new(),
            ),
            Err(JoinError::Panicked(_)) => (
                Err(io::Error::other("a compio TCP send task panicked")),
                Vec::new(),
            ),
        };
        buffer.clear();
        self.state = WriteState::Idle(buffer);
        Poll::Ready(result)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match self.state {
                WriteState::Idle(_) | WriteState::Sending(_) => {
                    ready!(self.poll_drain(cx))?;
                    let mut stream = self.stream.clone();
                    self.state =
                        WriteState::Closing(Box::pin(async move { stream.shutdown().await }));
                }
                WriteState::Closing(ref mut op) => {
                    let result = ready!(op.as_mut().poll(cx));
                    self.state = WriteState::Closed;
                    return Poll::Ready(result);
                }
                WriteState::Closed => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Bytes a `poll_write` accepted are the kernel's on a Tokio stream:
        // let the outstanding send finish rather than cancel it.
        if let WriteState::Sending(send) = mem::replace(&mut self.state, WriteState::Closed) {
            send.detach();
        }
    }
}

/// Send all of `buffer`, resubmitting the tail of every short send, and hand
/// the buffer back for reuse.
async fn send_all(mut stream: TcpStream, mut buffer: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
    let mut sent = 0;
    while sent < buffer.len() {
        let BufResult(result, slice) = stream
            .write(compio::buf::IoBufExt::slice(buffer, sent..))
            .await;
        buffer = slice.into_inner();
        match result {
            Ok(0) => return (Err(io::ErrorKind::WriteZero.into()), buffer),
            Ok(written) => sent += written,
            Err(error) => {
                tracing::debug!(%error, "tcp send failed");
                return (Err(error), buffer);
            }
        }
    }
    (Ok(()), buffer)
}

impl AsyncRead for CompioTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let result = self.get_mut().inner().reader.poll_read(cx, buf);
        if let Poll::Ready(Ok(read)) = &result {
            tracing::trace!(read, "tcp read");
        }
        result
    }
}

impl AsyncWrite for CompioTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = self.get_mut().inner().writer.poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &result {
            tracing::trace!(written, "tcp write");
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().inner().writer.poll_drain(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().inner().writer.poll_close(cx)
    }
}
