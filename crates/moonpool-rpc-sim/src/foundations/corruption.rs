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
        // Rare per write, and only where live traffic flows.
        let corrupt = self.enabled
            && self.written >= HANDSHAKE_BYTES
            && !buf.is_empty()
            && moonpool_sim::buggify_with_prob!(0.02);
        let poll = if corrupt {
            let mut copy = buf.to_vec();
            let byte = self.random.random_range(0..copy.len());
            let bit = self.random.random_range(0..8u32);
            copy[byte] ^= 1 << bit;
            assert_reachable!("rpc harness corrupted a live-session write");
            Pin::new(&mut self.inner).poll_write(cx, &copy)
        } else {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        };
        if let Poll::Ready(Ok(written)) = &poll {
            self.written += written;
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
