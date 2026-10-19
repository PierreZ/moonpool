//! TCP on the compio runtime: [`CompioNetworkProvider`],
//! [`CompioTcpListener`] and [`CompioTcpStream`].

use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use compio::io::compat::AsyncStream;
use compio::net::{TcpListener, TcpStream};
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

/// A TCP stream implementing `futures::io::{AsyncRead, AsyncWrite}`.
///
/// It is compio's completion-based socket behind compio's own compat layer
/// (`compio::io::compat::AsyncStream`), which costs one copy in each
/// direction: reads land in an internal buffer (8 KiB to start) and are
/// copied out, and writes are copied into a buffer that grows on demand.
///
/// **Bytes leave on flush.** A `poll_write` that returns `Ready` has only
/// buffered its bytes; they go to the socket when the buffer is flushed with
/// `poll_flush` or `poll_close`, or when it fills past two thirds of its
/// capacity. Protocol code that writes and then waits for an answer must
/// flush (moonpool-rpc flushes whenever its send queue drains, and hyper
/// flushes after every message), which is the `futures::io` contract anyway.
///
/// The stream is bound to the runtime thread that created it: polling or
/// dropping it on another thread panics. See the crate documentation.
pub struct CompioTcpStream {
    inner: SendWrapper<Pin<Box<AsyncStream<TcpStream>>>>,
}

impl CompioTcpStream {
    fn new(stream: TcpStream) -> Self {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "TCP_NODELAY not set; the connection is used as it is");
        }
        Self {
            inner: SendWrapper::new(Box::pin(AsyncStream::new(stream))),
        }
    }

    /// The compat stream, on the thread that owns it.
    fn stream(&mut self) -> Pin<&mut AsyncStream<TcpStream>> {
        self.inner
            .get_mut()
            .expect("a compio TCP stream was used off the runtime thread that created it")
            .as_mut()
    }
}

impl fmt::Debug for CompioTcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompioTcpStream").finish_non_exhaustive()
    }
}

impl AsyncRead for CompioTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let result = self.get_mut().stream().poll_read(cx, buf);
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
        let result = self.get_mut().stream().poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &result {
            tracing::trace!(written, "tcp write");
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().stream().poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().stream().poll_close(cx)
    }
}
