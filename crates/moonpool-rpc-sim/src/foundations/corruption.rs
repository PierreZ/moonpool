//! A session upgrade that corrupts live traffic, through the RPC crate's
//! `Connector` / `Acceptor` seam.
//!
//! The network's own bit-flip fault is the realistic source of corruption,
//! but its per-send rate (even spiked by `Chaos::BuggifyKnobs`) meets an
//! established RPC session on only a few seeds in hundreds. This wrapper is
//! a BUGGIFY site at the boundary the transport cannot see past: once the
//! handshake bytes are out, a write may leave with one bit flipped. The
//! receiving side's frame checksum must catch it, close the session, and
//! fail the calls it carried through the ordinary disconnect path; the
//! handler-side "exact request body" assertion proves nothing corrupt was
//! ever delivered.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncWrite};
use moonpool_rpc::{Acceptor, Connector, PeerContext};
use moonpool_sim::{RandomProvider, SimRandomProvider, assert_reachable};

/// Bytes a session writes before its `Hello` is out (frame header plus the
/// fixed `Hello` envelope): corruption is only injected after them.
const HANDSHAKE_BYTES: usize =
    moonpool_rpc::protocol::HEADER_LEN + moonpool_rpc::protocol::HELLO_ENVELOPE_LEN;

/// The corrupting upgrade.
#[derive(Clone)]
pub struct CorruptingWire {
    random: SimRandomProvider,
    enabled: bool,
}

impl CorruptingWire {
    /// Draw corruption positions from `random` (the simulation stream);
    /// `enabled: false` makes it a plain pass-through.
    #[must_use]
    pub fn new(random: SimRandomProvider, enabled: bool) -> Self {
        Self { random, enabled }
    }

    fn wrap<S>(&self, stream: S, peer: &str) -> (CorruptingStream<S>, PeerContext) {
        (
            CorruptingStream {
                inner: stream,
                written: 0,
                flip: None,
                random: self.random.clone(),
                enabled: self.enabled,
            },
            PeerContext::new(peer),
        )
    }
}

impl<S> Connector<S> for CorruptingWire
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = CorruptingStream<S>;

    async fn connect(&self, stream: S, peer: &str) -> io::Result<(Self::Stream, PeerContext)> {
        Ok(self.wrap(stream, peer))
    }
}

impl<S> Acceptor<S> for CorruptingWire
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = CorruptingStream<S>;

    async fn accept(&self, stream: S, peer: &str) -> io::Result<(Self::Stream, PeerContext)> {
        Ok(self.wrap(stream, peer))
    }
}

/// A stream whose writes may flip one bit after the handshake.
pub struct CorruptingStream<S> {
    inner: S,
    written: usize,
    /// A drawn flip (byte offset into the next write, bit) that has not
    /// gone out yet: kept across a `Pending` write and a short write, so a
    /// retry carries the same flip and no new draw is made.
    flip: Option<(usize, u32)>,
    random: SimRandomProvider,
    enabled: bool,
}

impl<S: AsyncRead + Unpin> AsyncRead for CorruptingStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CorruptingStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Pin::new(&mut self.inner).poll_write(cx, buf);
        }
        // Rare per write, and only where live traffic flows.
        if self.flip.is_none()
            && self.enabled
            && self.written >= HANDSHAKE_BYTES
            && moonpool_sim::buggify_with_prob!(0.02)
        {
            let byte = self.random.random_range(0..buf.len());
            let bit = self.random.random_range(0..8u32);
            self.flip = Some((byte, bit));
        }
        let Some((byte, bit)) = self.flip else {
            let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
            if let Poll::Ready(Ok(written)) = &poll {
                self.written += written;
            }
            return poll;
        };
        // A retried write may be shorter than the one the flip was drawn
        // for: keep the flip inside it.
        let byte = byte.min(buf.len() - 1);
        let mut copy = buf.to_vec();
        copy[byte] ^= 1 << bit;
        let poll = Pin::new(&mut self.inner).poll_write(cx, &copy);
        match &poll {
            Poll::Ready(Ok(written)) if *written > byte => {
                // The flipped byte went out.
                self.flip = None;
                self.written += written;
                assert_reachable!("rpc harness corrupted a live-session write");
            }
            Poll::Ready(Ok(written)) => {
                // A short write stopped before the flipped byte: the rest of
                // the buffer comes again, the flip moves with it.
                self.flip = Some((byte - written, bit));
                self.written += written;
            }
            Poll::Ready(Err(_)) => self.flip = None,
            Poll::Pending => {}
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures::io::AsyncWrite;
    use moonpool_sim::SimRandomProvider;

    use super::CorruptingStream;

    /// A writer that is first not ready, then takes at most `chunk` bytes
    /// per write.
    struct Slow {
        out: Vec<u8>,
        pending: bool,
        chunk: usize,
    }

    impl AsyncWrite for Slow {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if std::mem::take(&mut self.pending) {
                return Poll::Pending;
            }
            let take = buf.len().min(self.chunk);
            self.out.extend_from_slice(&buf[..take]);
            Poll::Ready(Ok(take))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A drawn flip survives a `Pending` write and a short write that stops
    /// before it, and then goes out exactly once, at its byte.
    #[test]
    fn a_flip_goes_out_once_after_pending_and_short_writes() {
        let mut stream = CorruptingStream {
            inner: Slow {
                out: Vec::new(),
                pending: true,
                chunk: 3,
            },
            written: 0,
            flip: Some((4, 0)),
            random: SimRandomProvider::new(),
            enabled: false,
        };
        let data = [0u8; 8];
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        let mut sent = 0;
        let mut pendings = 0;
        while sent < data.len() {
            match Pin::new(&mut stream).poll_write(&mut cx, &data[sent..]) {
                Poll::Ready(Ok(written)) => sent += written,
                Poll::Ready(Err(error)) => panic!("write failed: {error}"),
                Poll::Pending => pendings += 1,
            }
        }
        assert_eq!(pendings, 1);
        assert!(stream.flip.is_none());
        assert_eq!(stream.inner.out, [0, 0, 0, 0, 1, 0, 0, 0]);
    }
}
