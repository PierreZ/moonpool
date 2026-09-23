//! One TCP session: its bounded outbound queues, its liveness bookkeeping and
//! its read/write loops.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

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
    /// Nothing arrived within the ping timeout.
    PingTimeout,
    /// Closed from this side (control budget exceeded, a reply that cannot
    /// be framed, runtime policy).
    Local,
    /// Closed from this side after carrying nothing for the idle timeout.
    /// Not a failure.
    Idle,
    /// This side's selected connection to a peer, replaced by a session the
    /// peer dialed. Not a failure.
    Replaced,
}

impl CloseReason {
    /// Whether the connection ended because something went wrong (as
    /// opposed to an idle close or a tie-break), so the failure monitor
    /// and the reconnect backoff should count it.
    pub(crate) fn is_failure(&self) -> bool {
        !matches!(self, Self::Idle | Self::Replaced)
    }
}

/// Which side opened the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// Dialed by this runtime.
    Outbound,
    /// Accepted by this runtime's listener.
    Inbound,
}

/// What a queued frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameKind {
    /// Handshake, reply, rejection, ping or pong.
    Control,
    /// A request: of the given pending call, or one-way (`None`).
    Request(Option<u64>),
}

/// A frame queued for writing.
pub(crate) struct Outgoing {
    pub(crate) bytes: Vec<u8>,
    pub(crate) kind: FrameKind,
}

/// Why a frame could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueRefusal {
    Full,
    Closed,
}

/// A queued request frame and the call it belongs to (`None`: one-way).
pub(crate) type QueuedRequest = (Vec<u8>, Option<u64>);

struct QueueState {
    control: VecDeque<Vec<u8>>,
    requests: VecDeque<QueuedRequest>,
    /// Requests are written only once the peer's handshake was accepted, so
    /// a session refused at the handshake never carried a request.
    established: bool,
    /// The first reason the connection was closed for, once closed.
    closed: Option<CloseReason>,
    /// What the upgrade learned about the peer (set before any frame is read).
    peer_context: Option<PeerContext>,
    /// What the peer's `Hello` announced (set when the session establishes).
    peer_hello: Option<PeerHello>,
    /// The peer (canonical address) this connection is, or was, the
    /// selected connection to: the dialed address, or the listen address of
    /// an adopted inbound session. `None` for an inbound session this
    /// runtime only serves.
    peer_address: Option<SocketAddr>,
    /// Frames received so far (liveness).
    received: u64,
    /// When the last frame arrived.
    last_received: Duration,
    /// When a request or reply last went through (idleness).
    last_used: Duration,
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
    direction: Direction,
    queue: Mutex<QueueState>,
    max_requests: usize,
    max_control: usize,
    waker: AtomicWaker,
    /// Requests admitted from this connection whose reply handle has not
    /// finished yet.
    outstanding: AtomicUsize,
    counters: Arc<Counters>,
}

impl Connection {
    /// A connection whose first queued frame is `hello`, created at `now`.
    pub(crate) fn new(
        id: u64,
        peer: String,
        direction: Direction,
        hello: Vec<u8>,
        limits: (usize, usize),
        now: Duration,
        counters: &Arc<Counters>,
    ) -> Self {
        let (max_requests, max_control) = limits;
        counters.live_connections.fetch_add(1, Ordering::Relaxed);
        let mut control = VecDeque::new();
        control.push_back(hello);
        Self {
            id,
            peer,
            direction,
            queue: Mutex::new(QueueState {
                control,
                requests: VecDeque::new(),
                established: false,
                closed: None,
                peer_context: None,
                peer_hello: None,
                peer_address: None,
                received: 0,
                last_received: now,
                last_used: now,
            }),
            max_requests,
            // The handshake frame does not count against the reserve.
            max_control: max_control.saturating_add(1),
            waker: AtomicWaker::new(),
            outstanding: AtomicUsize::new(0),
            counters: Arc::clone(counters),
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn peer(&self) -> &str {
        &self.peer
    }

    pub(crate) fn direction(&self) -> Direction {
        self.direction
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.queue
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// Queue a request frame (`call_id` `None` for a one-way request).
    pub(crate) fn push_request(
        &self,
        frame: Vec<u8>,
        call_id: Option<u64>,
        now: Duration,
    ) -> Result<(), QueueRefusal> {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return Err(QueueRefusal::Closed);
        }
        if queue.requests.len() >= self.max_requests {
            return Err(QueueRefusal::Full);
        }
        queue.requests.push_back((frame, call_id));
        queue.last_used = queue.last_used.max(now);
        drop(queue);
        self.waker.wake();
        Ok(())
    }

    /// Queue frames that were admitted once already (moved from a replaced
    /// connection, or retained requests sent again), without the request
    /// cap: they must not be refused now. Returns `false` once closed.
    pub(crate) fn adopt_requests(&self, moved: Vec<QueuedRequest>, now: Duration) -> bool {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return false;
        }
        queue.requests.extend(moved);
        queue.last_used = queue.last_used.max(now);
        drop(queue);
        self.waker.wake();
        true
    }

    /// Take every request frame not yet written, in order.
    pub(crate) fn take_requests(&self) -> Vec<QueuedRequest> {
        self.lock().requests.drain(..).collect()
    }

    /// Queue a control frame (reply, rejection, ping, pong). Returns whether
    /// it was queued. Exceeding the control reserve closes the connection:
    /// a reply is never dropped while the session looks healthy.
    pub(crate) fn push_control(&self, frame: Vec<u8>) -> bool {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return false;
        }
        if queue.control.len() >= self.max_control {
            drop(queue);
            tracing::warn!(peer = %self.peer, "rpc control reserve exhausted, closing connection");
            self.close(CloseReason::Local);
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
    pub(crate) fn establish(&self, hello: PeerHello, now: Duration) {
        let mut queue = self.lock();
        queue.established = true;
        queue.peer_hello = Some(hello);
        queue.last_used = queue.last_used.max(now);
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

    /// Mark this connection as the selected connection to `address`.
    pub(crate) fn select_for(&self, address: SocketAddr) {
        self.lock().peer_address = Some(address);
    }

    /// The peer this connection is (or was) selected for.
    pub(crate) fn peer_address(&self) -> Option<SocketAddr> {
        self.lock().peer_address
    }

    /// A frame arrived at `now`.
    pub(crate) fn note_received(&self, now: Duration) {
        let mut queue = self.lock();
        queue.received += 1;
        queue.last_received = queue.last_received.max(now);
    }

    /// A request or reply went through at `now` (not a ping).
    pub(crate) fn note_used(&self, now: Duration) {
        let mut queue = self.lock();
        queue.last_used = queue.last_used.max(now);
    }

    /// Frames received so far.
    pub(crate) fn received(&self) -> u64 {
        self.lock().received
    }

    /// One more admitted request awaits its reply handle.
    pub(crate) fn begin_outstanding(&self) {
        self.outstanding.fetch_add(1, Ordering::Relaxed);
    }

    /// An admitted request's reply handle finished.
    pub(crate) fn end_outstanding(&self) {
        self.outstanding.fetch_sub(1, Ordering::Relaxed);
    }

    /// Close if idle at `now`: nothing queued to write, no reply owed, and
    /// no traffic for `idle` — measured from the last request or reply
    /// when `selected`, from the last received frame otherwise (an inbound
    /// session this side only serves). The caller has checked that no
    /// pending call rides on it. Returns whether it closed.
    pub(crate) fn close_if_idle(&self, now: Duration, idle: Duration, selected: bool) -> bool {
        let mut queue = self.lock();
        let since = if selected {
            queue.last_used
        } else {
            queue.last_received
        };
        let idle_now = queue.closed.is_none()
            && queue.established
            && queue.requests.is_empty()
            && self.outstanding.load(Ordering::Relaxed) == 0
            && now.saturating_sub(since) >= idle;
        if idle_now {
            queue.closed = Some(CloseReason::Idle);
            queue.control.clear();
        }
        drop(queue);
        if idle_now {
            self.waker.wake();
        }
        idle_now
    }

    /// Withdraw the still-unsent request frame of `call_id`. Returns whether
    /// it was withdrawn; `false` means the writer already took it (or the
    /// session closed), so it may have been transmitted.
    pub(crate) fn retract(&self, call_id: u64) -> bool {
        let mut queue = self.lock();
        let before = queue.requests.len();
        queue
            .requests
            .retain(|(_, queued)| *queued != Some(call_id));
        queue.requests.len() != before
    }

    /// Close the session for `reason`: no more frames are accepted and the
    /// writer stops. Returns the reason it is closed for, which is the
    /// first one recorded.
    pub(crate) fn close(&self, reason: CloseReason) -> CloseReason {
        let mut queue = self.lock();
        let reason = queue.closed.get_or_insert(reason).clone();
        queue.control.clear();
        queue.requests.clear();
        drop(queue);
        self.waker.wake();
        reason
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed.is_some()
    }

    /// Next frame to write, control first; `None` once closed.
    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Option<Outgoing>> {
        self.waker.register(cx.waker());
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return Poll::Ready(None);
        }
        if let Some(bytes) = queue.control.pop_front() {
            return Poll::Ready(Some(Outgoing {
                bytes,
                kind: FrameKind::Control,
            }));
        }
        if !queue.established {
            return Poll::Pending;
        }
        if let Some((bytes, call_id)) = queue.requests.pop_front() {
            return Poll::Ready(Some(Outgoing {
                bytes,
                kind: FrameKind::Request(call_id),
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
    admit_transmit: impl Fn(Option<u64>, usize) -> bool,
) -> CloseReason {
    loop {
        let next = futures::future::poll_fn(|cx| connection.poll_next(cx)).await;
        let Some(outgoing) = next else {
            return CloseReason::Local;
        };
        let admitted = match outgoing.kind {
            FrameKind::Control => true,
            FrameKind::Request(call_id) => admit_transmit(call_id, outgoing.bytes.len()),
        };
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
