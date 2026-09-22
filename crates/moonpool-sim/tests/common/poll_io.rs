//! Poll a stream exactly once with a no-op waker.
//!
//! Shared by the test binaries that include it with `#[path]`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use futures::io::{AsyncRead, AsyncWrite};

/// One `poll_write` of `data`, with a no-op waker.
pub fn poll_write_once(
    stream: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
) -> Poll<io::Result<usize>> {
    Pin::new(stream).poll_write(&mut Context::from_waker(Waker::noop()), data)
}

/// One `poll_read` into `data`, with a no-op waker.
pub fn poll_read_once(
    stream: &mut (impl AsyncRead + Unpin),
    data: &mut [u8],
) -> Poll<io::Result<usize>> {
    Pin::new(stream).poll_read(&mut Context::from_waker(Waker::noop()), data)
}
