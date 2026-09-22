//! One TCP session: its bounded outbound queues and its read/write loops.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures::task::AtomicWaker;

use super::upgrade::PeerContext;
use crate::stats::Counters;

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// Outbound connect failed or timed out: nothing was transmitted.
    ConnectFailed(String),
    /// The peer closed the stream cleanly between frames.
    PeerClosed,
    /// A socket read or write failed.
    Io(String),
    /// Framing, envelope or handshake violation.
    Protocol(String),
    /// A frame failed its checksum: corruption.
    Checksum(String),
    /// The peer speaks no protocol version this build supports.
    Version(String),
    /// Closed from this side (control budget exceeded, runtime policy).
    Local,
}

/// A frame queued for writing.
pub(crate) struct Outgoing {
    pub(crate) bytes: Vec<u8>,
    /// The call a request frame belongs to (marked transmitted when popped).
    pub(crate) call_id: Option<u64>,
}

/// Why a frame could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueRefusal {
    Full,
    Closed,
}

struct QueueState {
    control: VecDeque<Vec<u8>>,
    requests: VecDeque<(Vec<u8>, u64)>,
    /// Requests are written only once the peer's handshake was accepted, so
    /// a session refused at the handshake never carried a request.
    established: bool,
    closed: bool,
    /// What the upgrade learned about the peer (set before any frame is read).
    peer_context: Option<PeerContext>,
    /// What the peer's `Hello` announced (set when the session establishes).
    peer_hello: Option<PeerHello>,
}

/// What a peer announced in its `Hello`, as accepted by this side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeerHello {
    /// The peer runtime's incarnation.
    pub(crate) incarnation: crate::endpoint::Incarnation,
    /// The negotiated envelope version.
    pub(crate) version: u16,
    /// The largest frame payload the peer accepts.
    pub(crate) max_frame_bytes: u32,
}

/// One session's shared half: what callers and responders queue into and
/// what the connection's driver writes out.
pub(crate) struct Connection {
    id: u64,
    peer: String,
    queue: Mutex<QueueState>,
    max_requests: usize,
    max_control: usize,
    waker: AtomicWaker,
    counters: Arc<Counters>,
}

impl Connection {
    /// A connection whose first queued frame is `hello`.
    pub(crate) fn new(
        id: u64,
        peer: String,
        hello: Vec<u8>,
        max_requests: usize,
        max_control: usize,
        counters: &Arc<Counters>,
    ) -> Self {
        counters.live_connections.fetch_add(1, Ordering::Relaxed);
        let mut control = VecDeque::new();
        control.push_back(hello);
        Self {
            id,
            peer,
            queue: Mutex::new(QueueState {
                control,
                requests: VecDeque::new(),
                established: false,
                closed: false,
                peer_context: None,
                peer_hello: None,
            }),
            max_requests,
            // The handshake frame does not count against the reserve.
            max_control: max_control.saturating_add(1),
            waker: AtomicWaker::new(),
            counters: Arc::clone(counters),
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn peer(&self) -> &str {
        &self.peer
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.queue
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// Queue a request frame for `call_id`.
    pub(crate) fn push_request(&self, frame: Vec<u8>, call_id: u64) -> Result<(), QueueRefusal> {
        let mut queue = self.lock();
        if queue.closed {
            return Err(QueueRefusal::Closed);
        }
        if queue.requests.len() >= self.max_requests {
            return Err(QueueRefusal::Full);
        }
        queue.requests.push_back((frame, call_id));
        drop(queue);
        self.waker.wake();
        Ok(())
    }

    /// Queue a control frame (reply or rejection). Returns whether it was
    /// queued. Exceeding the control reserve closes the connection: a reply
    /// is never dropped while the session looks healthy.
    pub(crate) fn push_control(&self, frame: Vec<u8>) -> bool {
        let mut queue = self.lock();
        if queue.closed {
            return false;
        }
        if queue.control.len() >= self.max_control {
            drop(queue);
            tracing::warn!(peer = %self.peer, "rpc control reserve exhausted, closing connection");
            self.close();
            return false;
        }
        queue.control.push_back(frame);
        drop(queue);
        self.waker.wake();
        true
    }

    /// Record what the upgrade established about the peer.
    pub(crate) fn set_peer_context(&self, context: PeerContext) {
        self.lock().peer_context = Some(context);
    }

    /// What the upgrade established about the peer, once it ran.
    pub(crate) fn peer_context(&self) -> Option<PeerContext> {
        self.lock().peer_context.clone()
    }

    /// The peer's handshake was accepted: requests may flow.
    pub(crate) fn establish(&self, hello: PeerHello) {
        let mut queue = self.lock();
        queue.established = true;
        queue.peer_hello = Some(hello);
        drop(queue);
        self.waker.wake();
    }

    /// What the peer announced, once the session is established.
    pub(crate) fn peer_hello(&self) -> Option<PeerHello> {
        self.lock().peer_hello
    }

    pub(crate) fn is_established(&self) -> bool {
        self.lock().established
    }

    /// Withdraw the still-unsent request frame of `call_id`. Returns whether
    /// it was withdrawn; `false` means the writer already took it (or the
    /// session closed), so it may have been transmitted.
    pub(crate) fn retract(&self, call_id: u64) -> bool {
        let mut queue = self.lock();
        let before = queue.requests.len();
        queue.requests.retain(|(_, queued)| *queued != call_id);
        queue.requests.len() != before
    }

    /// Close the session: no more frames are accepted and the writer stops.
    pub(crate) fn close(&self) {
        let mut queue = self.lock();
        queue.closed = true;
        queue.control.clear();
        queue.requests.clear();
        drop(queue);
        self.waker.wake();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Next frame to write, control first; `None` once closed.
    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Option<Outgoing>> {
        self.waker.register(cx.waker());
        let mut queue = self.lock();
        if queue.closed {
            return Poll::Ready(None);
        }
        if let Some(bytes) = queue.control.pop_front() {
            return Poll::Ready(Some(Outgoing {
                bytes,
                call_id: None,
            }));
        }
        if !queue.established {
            return Poll::Pending;
        }
        if let Some((bytes, call_id)) = queue.requests.pop_front() {
            return Poll::Ready(Some(Outgoing {
                bytes,
                call_id: Some(call_id),
            }));
        }
        Poll::Pending
    }

    /// Nothing is ready to write right now (requests wait for the handshake).
    fn is_drained(&self) -> bool {
        let queue = self.lock();
        queue.control.is_empty() && (queue.requests.is_empty() || !queue.established)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.counters
            .live_connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}

/// Write queued frames until the connection is closed or a write fails.
///
/// `admit_transmit` runs for a request frame before its first byte is
/// written and decides whether it goes out (from then on the server may
/// execute it); a refused frame is skipped and its call completed by the
/// callback.
pub(crate) async fn write_loop<W: AsyncWrite + Unpin>(
    connection: &Connection,
    mut writer: W,
    admit_transmit: impl Fn(u64, usize) -> bool,
) -> CloseReason {
    loop {
        let next = futures::future::poll_fn(|cx| connection.poll_next(cx)).await;
        let Some(outgoing) = next else {
            return CloseReason::Local;
        };
        let admitted = outgoing
            .call_id
            .is_none_or(|call_id| admit_transmit(call_id, outgoing.bytes.len()));
        if admitted && let Err(error) = writer.write_all(&outgoing.bytes).await {
            return CloseReason::Io(error.to_string());
        }
        // Flush whenever nothing else is ready, including after a skipped
        // frame, so earlier writes never sit in a buffer.
        if connection.is_drained()
            && let Err(error) = writer.flush().await
        {
            return CloseReason::Io(error.to_string());
        }
    }
}

/// Read and hand every complete frame payload to `on_frame` until the
/// stream ends or a frame is refused.
pub(crate) async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    max_frame_bytes: u32,
    chunk_bytes: usize,
    mut on_frame: impl FnMut(Vec<u8>) -> Result<(), CloseReason>,
) -> CloseReason {
    let mut decoder = crate::protocol::FrameDecoder::new(max_frame_bytes);
    let mut chunk = vec![0; chunk_bytes.max(1)];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(read) => read,
            Err(error) => return CloseReason::Io(error.to_string()),
        };
        if read == 0 {
            return if decoder.is_idle() {
                CloseReason::PeerClosed
            } else {
                CloseReason::Protocol(format!(
                    "stream ended inside a frame ({} bytes buffered)",
                    decoder.buffered()
                ))
            };
        }
        decoder.feed(&chunk[..read]);
        loop {
            match decoder.next_frame() {
                Ok(Some(payload)) => {
                    if let Err(reason) = on_frame(payload) {
                        return reason;
                    }
                }
                Ok(None) => break,
                Err(error @ crate::protocol::FrameError::Checksum { .. }) => {
                    return CloseReason::Checksum(error.to_string());
                }
                Err(error) => return CloseReason::Protocol(error.to_string()),
            }
        }
    }
}
