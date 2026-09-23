//! A session upgrade whose reads can be stalled, through the RPC crate's
//! `Connector` / `Acceptor` seam.
//!
//! The RPC reader always drains its socket, so a consumer that stops
//! taking items never stops the network: credit runs out, but the
//! producer's writer never backs up. Stalling the consumer's reads does.
//! The simulated TCP stream has an end-to-end send window that only the
//! peer's reads release, so while the consumer reads nothing the
//! producer's writes return `Pending` once the window is full, and every
//! further frame waits in the producer's connection queue. The other
//! direction keeps flowing: the consumer's acknowledgements, cancels,
//! pings and requests still reach the producer.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures::io::{AsyncRead, AsyncWrite};
use moonpool_rpc::{Acceptor, Connector, PeerContext};

/// The shared switch: stalled or not, and the readers waiting on it.
#[derive(Default)]
struct Switch {
    stalled: AtomicBool,
    readers: Mutex<Vec<Waker>>,
}

/// The stallable upgrade; clones share one switch.
#[derive(Clone, Default)]
pub struct ReadStall {
    switch: Arc<Switch>,
}

impl ReadStall {
    /// Stop every session's reads until the returned guard is dropped.
    #[must_use]
    pub fn stall(&self) -> Stalled {
        self.switch.stalled.store(true, Ordering::SeqCst);
        Stalled {
            switch: Arc::clone(&self.switch),
        }
    }

    fn wrap<S>(&self, stream: S, peer: &str) -> (StallingStream<S>, PeerContext) {
        (
            StallingStream {
                inner: stream,
                switch: Arc::clone(&self.switch),
            },
            PeerContext::new(peer),
        )
    }
}

/// Reads stay stalled while this lives.
pub struct Stalled {
    switch: Arc<Switch>,
}

impl Drop for Stalled {
    fn drop(&mut self) {
        self.switch.stalled.store(false, Ordering::SeqCst);
        let readers = std::mem::take(
            &mut *self
                .switch
                .readers
                .lock()
                .expect("Mutex poisoned: prior task panicked"),
        );
        for reader in readers {
            reader.wake();
        }
    }
}

impl<S> Connector<S> for ReadStall
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = StallingStream<S>;

    async fn connect(&self, stream: S, peer: &str) -> io::Result<(Self::Stream, PeerContext)> {
        Ok(self.wrap(stream, peer))
    }
}

impl<S> Acceptor<S> for ReadStall
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = StallingStream<S>;

    async fn accept(&self, stream: S, peer: &str) -> io::Result<(Self::Stream, PeerContext)> {
        Ok(self.wrap(stream, peer))
    }
}

/// A stream whose reads wait while its switch is stalled.
pub struct StallingStream<S> {
    inner: S,
    switch: Arc<Switch>,
}

impl<S: AsyncRead + Unpin> AsyncRead for StallingStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.switch.stalled.load(Ordering::SeqCst) {
            let mut readers = self
                .switch
                .readers
                .lock()
                .expect("Mutex poisoned: prior task panicked");
            // Checked again under the lock: a release in between has
            // already taken the wakers.
            if self.switch.stalled.load(Ordering::SeqCst) {
                if !readers.iter().any(|reader| reader.will_wake(cx.waker())) {
                    readers.push(cx.waker().clone());
                }
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for StallingStream<S> {
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

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}
