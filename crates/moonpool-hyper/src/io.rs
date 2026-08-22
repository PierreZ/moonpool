//! Bridge a futures-io stream into hyper's IO traits.
//!
//! hyper 1.x defines its own [`Read`](hyper::rt::Read) and
//! [`Write`](hyper::rt::Write) traits (forward-compatible with completion-based
//! IO), and ships no implementation for anything but tokio, via `hyper-util`'s
//! `TokioIo`. Every moonpool stream (`SimTcpStream`, and the tokio streams a
//! `NetworkProvider` hands out in production) already implements the futures-io
//! [`AsyncRead`](futures::io::AsyncRead) / [`AsyncWrite`](futures::io::AsyncWrite)
//! pair, so [`HyperIo`] adapts that shape directly, replacing the
//! `tokio_util::compat::Compat` plus `TokioIo` two-hop bridge.

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use futures::io::{AsyncRead, AsyncWrite};

/// Wraps a futures-io stream so hyper can read from and write to it.
///
/// Construct with [`HyperIo::new`] and hand the result to any hyper connection
/// builder. Vectored writes are opt-in through
/// [`with_vectored_writes`](HyperIo::with_vectored_writes).
#[derive(Debug)]
pub struct HyperIo<S> {
    inner: S,
    vectored_writes: bool,
}

impl<S> HyperIo<S> {
    /// Wrap a stream, reporting no vectored write support.
    ///
    /// futures-io offers no way to probe whether the underlying stream has an
    /// efficient `poll_write_vectored`, and the default forwarding
    /// implementation writes only the first non-empty slice, so claiming
    /// support that is not there would degrade throughput. Streams that do
    /// support it opt in with
    /// [`with_vectored_writes`](HyperIo::with_vectored_writes).
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            vectored_writes: false,
        }
    }

    /// Set what [`is_write_vectored`](hyper::rt::Write::is_write_vectored)
    /// reports to hyper.
    ///
    /// Pass `true` only for streams whose `poll_write_vectored` really writes
    /// from several buffers: hyper uses the answer to decide whether to hand
    /// down header and body slices separately instead of coalescing them.
    #[must_use]
    pub fn with_vectored_writes(mut self, vectored: bool) -> Self {
        self.vectored_writes = vectored;
        self
    }

    /// Borrow the wrapped stream.
    #[must_use]
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Mutably borrow the wrapped stream.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Consume the wrapper and return the stream.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: AsyncRead + Unpin> hyper::rt::Read for HyperIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // `initialize_unfilled` is safe: it zero-fills whatever part of the
        // unfilled region hyper has not yet initialized and hands back the
        // whole region as `&mut [u8]`. Handing uninitialized memory to
        // futures-io is impossible (its `poll_read` takes `&mut [u8]`), so
        // this zeroing is what the bridge costs, exactly as in
        // `tokio_util::compat::Compat`.
        let n = ready!(Pin::new(&mut self.inner).poll_read(cx, buf.initialize_unfilled()))?;

        // SAFETY: `advance(n)` requires that `n` more bytes of the unfilled
        // region are initialized and that `n` fits within it. Both hold: the
        // slice just passed to `poll_read` covers the entire unfilled region
        // and was fully initialized by `initialize_unfilled` above, and the
        // `AsyncRead` contract bounds a successful read by the length of that
        // slice, so `n <= buf.remaining()`. Nothing here de-initializes bytes.
        unsafe {
            buf.advance(n);
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> hyper::rt::Write for HyperIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // futures-io spells the half-close `poll_close`; hyper spells it
        // `poll_shutdown`. Same operation: finish the write side, leave the
        // read side to drain.
        Pin::new(&mut self.inner).poll_close(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.vectored_writes
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::mem::MaybeUninit;

    use futures::task::noop_waker_ref;
    use hyper::rt::{Read as _, ReadBuf, Write as _};

    use super::{AsyncRead, AsyncWrite, Context, HyperIo, IoSlice, Pin, Poll, io};

    /// One scripted answer from the fake stream's read side.
    enum ReadStep {
        /// Copy these bytes into the caller's buffer.
        Data(&'static [u8]),
        /// Report that no data is available yet.
        Pending,
        /// Report end of stream.
        Eof,
    }

    /// A futures-io stream whose reads follow a script and whose writes are
    /// recorded, so the bridge can be driven by hand with a noop waker.
    #[derive(Default)]
    struct Scripted {
        reads: VecDeque<ReadStep>,
        written: Vec<u8>,
        /// Number of slices handed to the last `poll_write_vectored` call.
        last_vectored: Option<usize>,
        closed: bool,
    }

    impl Scripted {
        fn with_reads(steps: impl IntoIterator<Item = ReadStep>) -> Self {
            Self {
                reads: steps.into_iter().collect(),
                ..Self::default()
            }
        }
    }

    impl AsyncRead for Scripted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            match self.reads.pop_front() {
                Some(ReadStep::Data(data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Poll::Ready(Ok(n))
                }
                Some(ReadStep::Pending) => Poll::Pending,
                Some(ReadStep::Eof) | None => Poll::Ready(Ok(0)),
            }
        }
    }

    impl AsyncWrite for Scripted {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            self.last_vectored = Some(bufs.len());
            let mut written = 0;
            for buf in bufs {
                self.written.extend_from_slice(buf);
                written += buf.len();
            }
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.closed = true;
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn read_fills_across_several_polls() {
        let mut stream = HyperIo::new(Scripted::with_reads([
            ReadStep::Data(b"abc"),
            ReadStep::Pending,
            ReadStep::Data(b"de"),
        ]));
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut backing = [0u8; 16];
        let mut buf = ReadBuf::new(&mut backing);

        assert!(matches!(
            Pin::new(&mut stream).poll_read(&mut cx, buf.unfilled()),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(buf.filled(), b"abc".as_slice());

        assert!(
            Pin::new(&mut stream)
                .poll_read(&mut cx, buf.unfilled())
                .is_pending()
        );
        assert_eq!(buf.filled(), b"abc".as_slice());

        assert!(matches!(
            Pin::new(&mut stream).poll_read(&mut cx, buf.unfilled()),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(buf.filled(), b"abcde".as_slice());
    }

    #[test]
    fn eof_leaves_the_cursor_unadvanced() {
        let mut stream = HyperIo::new(Scripted::with_reads([ReadStep::Eof]));
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut backing = [0u8; 8];
        let mut buf = ReadBuf::new(&mut backing);

        assert!(matches!(
            Pin::new(&mut stream).poll_read(&mut cx, buf.unfilled()),
            Poll::Ready(Ok(()))
        ));
        // hyper reads "nothing was filled" as end of stream.
        assert!(buf.filled().is_empty());
    }

    #[test]
    fn read_into_an_uninitialized_buffer() {
        let mut stream = HyperIo::new(Scripted::with_reads([ReadStep::Data(b"hi")]));
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut backing = [MaybeUninit::<u8>::uninit(); 8];
        let mut buf = ReadBuf::uninit(&mut backing);

        assert!(matches!(
            Pin::new(&mut stream).poll_read(&mut cx, buf.unfilled()),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(buf.filled(), b"hi".as_slice());
    }

    #[test]
    fn vectored_writes_reach_the_stream_as_slices() {
        let mut stream = HyperIo::new(Scripted::default()).with_vectored_writes(true);
        let mut cx = Context::from_waker(noop_waker_ref());

        assert!(stream.is_write_vectored());
        let bufs = [IoSlice::new(b"ab"), IoSlice::new(b"cd")];
        assert!(matches!(
            Pin::new(&mut stream).poll_write_vectored(&mut cx, &bufs),
            Poll::Ready(Ok(4))
        ));
        assert_eq!(stream.get_ref().last_vectored, Some(2));
        assert_eq!(stream.get_ref().written, b"abcd".as_slice());
    }

    #[test]
    fn vectored_support_is_off_unless_opted_in() {
        let off = HyperIo::new(Scripted::default());
        assert!(!off.is_write_vectored());
        assert!(
            HyperIo::new(Scripted::default())
                .with_vectored_writes(true)
                .is_write_vectored()
        );
    }

    #[test]
    fn shutdown_closes_the_write_side() {
        let mut stream = HyperIo::new(Scripted::default());
        let mut cx = Context::from_waker(noop_waker_ref());

        assert!(matches!(
            Pin::new(&mut stream).poll_shutdown(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert!(stream.get_ref().closed);
    }
}
