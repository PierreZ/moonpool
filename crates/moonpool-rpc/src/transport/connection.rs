//! One TCP session: its bounded outbound queues, its liveness bookkeeping and
//! its read/write loops.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures::task::AtomicWaker;

use super::upgrade::PeerContext;
use crate::protocol::{WireMessage, encode_frame, encode_message};
use crate::stats::Counters;
use crate::stream::producer::{StreamCore, Terminal};

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
    /// Handshake, rejection, ping, pong or a stream acknowledgement/cancel.
    Control,
    /// A request: of the given pending call, or one-way (`None`).
    Request(Option<u64>),
    /// A reply to an admitted request, or a stream item or end.
    Data,
}

/// Something released once its frame left the queue (written, or discarded
/// with the connection): the room an admitted request reserved, a stream
/// slot. Dropped outside every lock.
pub(crate) type Release = Box<dyn Send>;

/// A frame queued for writing.
pub(crate) struct Outgoing {
    pub(crate) bytes: Vec<u8>,
    pub(crate) kind: FrameKind,
    /// Released after the frame was written.
    pub(crate) release: Option<Release>,
}

/// Why a frame could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueRefusal {
    Full,
    Closed,
}

/// A queued request frame and the call it belongs to (`None`: one-way).
pub(crate) type QueuedRequest = (Vec<u8>, Option<u64>);

/// A data source the writer serves in turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Requests,
    Replies,
    Stream(u64),
}

/// A caller's pending stream signals, coalesced: only the latest
/// cumulative acknowledgement matters, and a cancel supersedes it.
#[derive(Debug, Default, Clone, Copy)]
struct Signal {
    ack: Option<u64>,
    cancel: bool,
}

/// One frame of a produced stream's outbound queue.
pub(crate) struct StreamFrame {
    pub(crate) bytes: Vec<u8>,
    pub(crate) release: Option<Release>,
}

/// The bounds of one connection's outbound queues.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueLimits {
    /// Request frames queued unwritten.
    pub(crate) requests: usize,
    /// Control frames queued unwritten (the handshake is extra).
    pub(crate) control: usize,
    /// Queued data bytes above which new requests are refused.
    pub(crate) bytes: u64,
    /// The same over the whole runtime.
    pub(crate) runtime_bytes: u64,
}

struct QueueState {
    control: VecDeque<Vec<u8>>,
    requests: VecDeque<QueuedRequest>,
    /// Replies to admitted requests; each carries the room its admission
    /// reserved.
    replies: VecDeque<(Vec<u8>, Release)>,
    /// Produced streams' items and ends, per stream, in order.
    stream_out: BTreeMap<u64, VecDeque<StreamFrame>>,
    /// Consumed streams' acknowledgements and cancels, coalesced.
    signals: BTreeMap<u64, Signal>,
    /// Data sources with queued frames, in the order the writer serves them.
    turns: VecDeque<Source>,
    /// Streams produced for requests that arrived on this connection, by
    /// the caller's call id (the stream id on this session).
    server_streams: BTreeMap<u64, Arc<StreamCore>>,
    /// Bytes of queued requests, replies and stream frames.
    queued_bytes: u64,
    /// Of those, bytes of queued requests: the only data this side's
    /// callers can add at will, so the only bytes the queue budgets refuse.
    request_bytes: u64,
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

impl QueueState {
    /// Put `source` in the writer's rotation if it is not there yet.
    fn enlist(&mut self, source: Source) {
        if !self.turns.contains(&source) {
            self.turns.push_back(source);
        }
    }

    /// Everything queued, taken out so it can be dropped outside the lock.
    fn drain_all(&mut self) -> Discarded {
        self.control.clear();
        self.signals.clear();
        self.turns.clear();
        self.queued_bytes = 0;
        self.request_bytes = 0;
        (
            std::mem::take(&mut self.requests),
            std::mem::take(&mut self.replies),
            std::mem::take(&mut self.stream_out),
        )
    }
}

/// Frames taken off a closed connection, dropped outside its lock.
type Discarded = (
    VecDeque<QueuedRequest>,
    VecDeque<(Vec<u8>, Release)>,
    BTreeMap<u64, VecDeque<StreamFrame>>,
);

/// What the peer announced in its `Hello`, as accepted by this side.
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
///
/// # Write order
///
/// Control frames first (handshake, pings, pongs, rejections), then the
/// coalesced acknowledgements and cancels of the streams this side
/// consumes, then one data frame per source in turn: the request queue,
/// the reply queue and each produced stream's queue. A busy stream or a
/// deep request queue therefore delays another stream's next item by at
/// most one frame per source, never by its whole backlog, and control
/// frames wait for at most the one data frame being written (bounded by
/// the frame limit): TCP keeps one byte stream, so that head-of-line wait
/// cannot be removed, only bounded.
pub(crate) struct Connection {
    id: u64,
    peer: String,
    direction: Direction,
    queue: Mutex<QueueState>,
    limits: QueueLimits,
    waker: AtomicWaker,
    /// Requests admitted from this connection whose reply (or stream end)
    /// is still owed or queued.
    outstanding: AtomicUsize,
    /// Windows of the streams produced on this connection, reserved from
    /// its producer budget.
    stream_windows: Arc<AtomicU64>,
    counters: Arc<Counters>,
}

impl Connection {
    /// A connection whose first queued frame is `hello`, created at `now`.
    pub(crate) fn new(
        id: u64,
        peer: String,
        direction: Direction,
        hello: Vec<u8>,
        limits: QueueLimits,
        now: Duration,
        counters: &Arc<Counters>,
    ) -> Self {
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
                replies: VecDeque::new(),
                stream_out: BTreeMap::new(),
                signals: BTreeMap::new(),
                turns: VecDeque::new(),
                server_streams: BTreeMap::new(),
                queued_bytes: 0,
                request_bytes: 0,
                established: false,
                closed: None,
                peer_context: None,
                peer_hello: None,
                peer_address: None,
                received: 0,
                last_received: now,
                last_used: now,
            }),
            limits: QueueLimits {
                // The handshake frame does not count against the reserve.
                control: limits.control.saturating_add(1),
                ..limits
            },
            waker: AtomicWaker::new(),
            outstanding: AtomicUsize::new(0),
            stream_windows: Arc::new(AtomicU64::new(0)),
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

    fn add_bytes(&self, queue: &mut QueueState, bytes: usize, request: bool) {
        let bytes = bytes as u64;
        queue.queued_bytes = queue.queued_bytes.saturating_add(bytes);
        self.counters
            .queued_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        if request {
            queue.request_bytes = queue.request_bytes.saturating_add(bytes);
            self.counters
                .queued_request_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn sub_bytes(&self, queue: &mut QueueState, bytes: u64, request: bool) {
        let total = bytes.min(queue.queued_bytes);
        queue.queued_bytes -= total;
        self.counters
            .queued_bytes
            .fetch_sub(total, Ordering::Relaxed);
        if request {
            let bytes = bytes.min(queue.request_bytes);
            queue.request_bytes -= bytes;
            self.counters
                .queued_request_bytes
                .fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    /// Give back every byte still counted (closing, or dropped).
    fn forget_bytes(&self, queue: &mut QueueState) {
        self.counters
            .queued_bytes
            .fetch_sub(queue.queued_bytes, Ordering::Relaxed);
        self.counters
            .queued_request_bytes
            .fetch_sub(queue.request_bytes, Ordering::Relaxed);
        queue.queued_bytes = 0;
        queue.request_bytes = 0;
    }

    /// Queue a request frame (`call_id` `None` for a one-way request),
    /// within the request count and the connection's and runtime's queued
    /// request byte budgets. Replies and stream items do not count against
    /// these budgets (their own admission bounds them), so a backlog of
    /// streams to one slow peer never refuses calls to another.
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
        let len = frame.len() as u64;
        let runtime = self.counters.queued_request_bytes.load(Ordering::Relaxed);
        if queue.requests.len() >= self.limits.requests
            || queue.request_bytes.saturating_add(len) > self.limits.bytes
            || runtime.saturating_add(len) > self.limits.runtime_bytes
        {
            return Err(QueueRefusal::Full);
        }
        self.add_bytes(&mut queue, frame.len(), true);
        queue.requests.push_back((frame, call_id));
        queue.enlist(Source::Requests);
        queue.last_used = queue.last_used.max(now);
        drop(queue);
        self.waker.wake();
        Ok(())
    }

    /// Queue frames that were admitted once already (moved from a replaced
    /// connection, or retained requests sent again), without the request
    /// cap or byte budgets: they must not be refused now. Returns `false`
    /// once closed.
    pub(crate) fn adopt_requests(&self, moved: Vec<QueuedRequest>, now: Duration) -> bool {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return false;
        }
        for (frame, call_id) in moved {
            self.add_bytes(&mut queue, frame.len(), true);
            queue.requests.push_back((frame, call_id));
        }
        if !queue.requests.is_empty() {
            queue.enlist(Source::Requests);
        }
        queue.last_used = queue.last_used.max(now);
        drop(queue);
        self.waker.wake();
        true
    }

    /// Take every request frame not yet written, in order.
    pub(crate) fn take_requests(&self) -> Vec<QueuedRequest> {
        let mut queue = self.lock();
        let taken: Vec<QueuedRequest> = queue.requests.drain(..).collect();
        let bytes = taken.iter().map(|(frame, _)| frame.len() as u64).sum();
        self.sub_bytes(&mut queue, bytes, true);
        queue.turns.retain(|source| *source != Source::Requests);
        taken
    }

    /// Queue a control frame (rejection, ping, pong). Returns whether it
    /// was queued. Exceeding the control reserve closes the connection: a
    /// rejection is never dropped while the session looks healthy.
    pub(crate) fn push_control(&self, frame: Vec<u8>) -> bool {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return false;
        }
        if queue.control.len() >= self.limits.control {
            drop(queue);
            tracing::warn!(peer = %self.peer, "rpc control reserve exhausted, closing connection");
            Counters::bump(&self.counters.control_reserve_closes);
            let _ = self.close(CloseReason::Local);
            return false;
        }
        queue.control.push_back(frame);
        drop(queue);
        self.waker.wake();
        true
    }

    /// Queue the reply to an admitted request. Never refused while open:
    /// `release` is the room its admission reserved, freed once written.
    /// Returns the release back when the connection is closed.
    pub(crate) fn push_reply(&self, frame: Vec<u8>, release: Release) -> Result<(), Release> {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return Err(release);
        }
        self.add_bytes(&mut queue, frame.len(), false);
        queue.replies.push_back((frame, release));
        queue.enlist(Source::Replies);
        drop(queue);
        self.waker.wake();
        Ok(())
    }

    /// Queue a frame of the stream `call_id` produced on this connection,
    /// after its earlier frames. Never refused while open: the stream's
    /// window, reserved at admission, bounds its items. The frame comes
    /// back once the connection is closed. The writer is not woken: the
    /// producer calls [`wake_writer`](Self::wake_writer) once it released
    /// its own locks.
    pub(crate) fn push_stream_frame(
        &self,
        call_id: u64,
        frame: StreamFrame,
    ) -> Result<(), StreamFrame> {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return Err(frame);
        }
        self.add_bytes(&mut queue, frame.bytes.len(), false);
        queue
            .stream_out
            .entry(call_id)
            .or_default()
            .push_back(frame);
        queue.enlist(Source::Stream(call_id));
        Ok(())
    }

    /// Wake the writer (after frames were queued without waking it).
    pub(crate) fn wake_writer(&self) {
        self.waker.wake();
    }

    /// Record a produced stream admitted on this connection. `false` when
    /// the connection closed, or the caller reused a live stream's id.
    pub(crate) fn register_stream(&self, call_id: u64, core: &Arc<StreamCore>) -> bool {
        let mut queue = self.lock();
        if queue.closed.is_some() || queue.server_streams.contains_key(&call_id) {
            return false;
        }
        queue.server_streams.insert(call_id, Arc::clone(core));
        true
    }

    /// Whether a produced stream with this id is live on the connection.
    pub(crate) fn has_stream(&self, call_id: u64) -> bool {
        self.lock().server_streams.contains_key(&call_id)
    }

    /// The live produced stream with this id.
    pub(crate) fn stream(&self, call_id: u64) -> Option<Arc<StreamCore>> {
        self.lock().server_streams.get(&call_id).cloned()
    }

    /// The windows reserved by streams produced on this connection.
    pub(crate) fn stream_windows(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.stream_windows)
    }

    /// Produced streams live on the connection.
    pub(crate) fn stream_count(&self) -> usize {
        self.lock().server_streams.len()
    }

    /// Forget a produced stream; with `discard`, also drop its frames not
    /// yet written (a cancelled stream). The removed frames are returned so
    /// they are dropped outside the lock.
    pub(crate) fn remove_stream(&self, call_id: u64, discard: bool) -> Vec<StreamFrame> {
        let mut queue = self.lock();
        queue.server_streams.remove(&call_id);
        if !discard {
            return Vec::new();
        }
        let frames: Vec<StreamFrame> = queue
            .stream_out
            .remove(&call_id)
            .map(Vec::from)
            .unwrap_or_default();
        let bytes = frames.iter().map(|frame| frame.bytes.len() as u64).sum();
        self.sub_bytes(&mut queue, bytes, false);
        queue
            .turns
            .retain(|source| *source != Source::Stream(call_id));
        frames
    }

    /// Take every produced stream (the connection ended).
    pub(crate) fn take_streams(&self) -> Vec<Arc<StreamCore>> {
        std::mem::take(&mut self.lock().server_streams)
            .into_values()
            .collect()
    }

    /// Acknowledge `consumed` bytes of the consumed stream `call_id`
    /// (coalesced with any acknowledgement not yet written).
    pub(crate) fn signal_ack(&self, call_id: u64, consumed: u64) {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return;
        }
        let signal = queue.signals.entry(call_id).or_default();
        if !signal.cancel {
            signal.ack = Some(signal.ack.map_or(consumed, |ack| ack.max(consumed)));
        }
        drop(queue);
        self.waker.wake();
    }

    /// Tell the producer the consumed stream `call_id` was abandoned.
    pub(crate) fn signal_cancel(&self, call_id: u64) {
        let mut queue = self.lock();
        if queue.closed.is_some() {
            return;
        }
        let signal = queue.signals.entry(call_id).or_default();
        signal.cancel = true;
        signal.ack = None;
        drop(queue);
        self.waker.wake();
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

    /// A request, reply or stream frame went through at `now` (not a ping).
    pub(crate) fn note_used(&self, now: Duration) {
        let mut queue = self.lock();
        queue.last_used = queue.last_used.max(now);
    }

    /// Frames received so far.
    pub(crate) fn received(&self) -> u64 {
        self.lock().received
    }

    /// How long nothing has arrived, at `now`.
    pub(crate) fn silent_for(&self, now: Duration) -> Duration {
        now.saturating_sub(self.lock().last_received)
    }

    /// One more admitted request awaits its reply; returns the new count.
    pub(crate) fn begin_outstanding(&self) -> usize {
        self.outstanding.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// An admitted request's reply was written or discarded.
    pub(crate) fn end_outstanding(&self) {
        self.outstanding.fetch_sub(1, Ordering::Relaxed);
    }

    /// Close if idle at `now`: nothing queued to write, no reply or stream
    /// owed, and no traffic for `idle` — measured from the last request or
    /// reply when `selected`, from the last received frame otherwise (an
    /// inbound session this side only serves). The caller has checked that
    /// no pending call rides on it. Returns whether it closed.
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
            && queue.replies.is_empty()
            && queue.stream_out.is_empty()
            && queue.signals.is_empty()
            && queue.server_streams.is_empty()
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
        let mut bytes = 0;
        queue.requests.retain(|(frame, queued)| {
            let keep = *queued != Some(call_id);
            if !keep {
                bytes += frame.len() as u64;
            }
            keep
        });
        if bytes == 0 {
            return false;
        }
        self.sub_bytes(&mut queue, bytes, true);
        if queue.requests.is_empty() {
            queue.turns.retain(|source| *source != Source::Requests);
        }
        true
    }

    /// Close the session for `reason`: no more frames are accepted and the
    /// writer stops. Returns the reason it is closed for, which is the
    /// first one recorded. Produced streams stay registered until
    /// [`take_streams`](Self::take_streams) ends them.
    pub(crate) fn close(&self, reason: CloseReason) -> CloseReason {
        let mut queue = self.lock();
        let reason = queue.closed.get_or_insert(reason).clone();
        self.forget_bytes(&mut queue);
        let discarded = queue.drain_all();
        drop(queue);
        drop(discarded);
        self.waker.wake();
        reason
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed.is_some()
    }

    /// Next frame to write (see the write order); `None` once closed.
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
                release: None,
            }));
        }
        if let Some((call_id, signal)) = queue.signals.pop_first() {
            let message = if signal.cancel {
                WireMessage::StreamCancel { call_id }
            } else {
                WireMessage::StreamAck {
                    call_id,
                    consumed: signal.ack.unwrap_or(0),
                }
            };
            let Ok(bytes) = encode_frame(&encode_message(&message), u32::MAX) else {
                // A fixed 17-byte envelope always frames. Never send a
                // silently empty signal: end the session instead.
                tracing::error!(peer = %self.peer, call_id, "rpc stream signal could not be framed");
                queue.closed.get_or_insert(CloseReason::Local);
                return Poll::Ready(None);
            };
            return Poll::Ready(Some(Outgoing {
                bytes,
                kind: FrameKind::Control,
                release: None,
            }));
        }
        for _ in 0..queue.turns.len() {
            let Some(source) = queue.turns.pop_front() else {
                break;
            };
            let next = match source {
                Source::Requests if !queue.established => {
                    queue.turns.push_back(source);
                    continue;
                }
                Source::Requests => queue.requests.pop_front().map(|(bytes, call_id)| {
                    let more = !queue.requests.is_empty();
                    (bytes, FrameKind::Request(call_id), None, more)
                }),
                Source::Replies => queue.replies.pop_front().map(|(bytes, release)| {
                    let more = !queue.replies.is_empty();
                    (bytes, FrameKind::Data, Some(release), more)
                }),
                Source::Stream(call_id) => {
                    let frames = queue.stream_out.get_mut(&call_id);
                    let frame = frames.and_then(VecDeque::pop_front);
                    let more = queue
                        .stream_out
                        .get(&call_id)
                        .is_some_and(|frames| !frames.is_empty());
                    if !more {
                        queue.stream_out.remove(&call_id);
                    }
                    frame.map(|frame| (frame.bytes, FrameKind::Data, frame.release, more))
                }
            };
            let Some((bytes, kind, release, more)) = next else {
                continue;
            };
            if more {
                queue.turns.push_back(source);
            }
            let request = matches!(kind, FrameKind::Request(_));
            self.sub_bytes(&mut queue, bytes.len() as u64, request);
            return Poll::Ready(Some(Outgoing {
                bytes,
                kind,
                release,
            }));
        }
        Poll::Pending
    }

    /// Nothing is ready to write right now (requests wait for the handshake).
    fn is_drained(&self) -> bool {
        let queue = self.lock();
        queue.control.is_empty()
            && queue.signals.is_empty()
            && queue.replies.is_empty()
            && queue.stream_out.is_empty()
            && (queue.requests.is_empty() || !queue.established)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.counters
            .live_connections
            .fetch_sub(1, Ordering::Relaxed);
        let counters = Arc::clone(&self.counters);
        if let Ok(queue) = self.queue.get_mut() {
            counters
                .queued_bytes
                .fetch_sub(queue.queued_bytes, Ordering::Relaxed);
            counters
                .queued_request_bytes
                .fetch_sub(queue.request_bytes, Ordering::Relaxed);
            queue.queued_bytes = 0;
            queue.request_bytes = 0;
            // The runtime is gone: wake every producer of this connection.
            for core in std::mem::take(&mut queue.server_streams).into_values() {
                core.terminate(Terminal::Shutdown);
            }
        }
    }
}

/// Resolve after being polled once: lets the executor run other tasks.
async fn yield_now() {
    let mut yielded = false;
    futures::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

/// Write queued frames until the connection is closed or a write fails,
/// yielding after every `batch` frames written back to back.
///
/// `admit_transmit` runs for a request frame before its first byte is
/// written and decides whether it goes out (from then on the server may
/// execute it); a refused frame is skipped and its call completed by the
/// callback.
pub(crate) async fn write_loop<W: AsyncWrite + Unpin>(
    connection: &Connection,
    mut writer: W,
    batch: usize,
    admit_transmit: impl Fn(Option<u64>, usize) -> bool,
) -> CloseReason {
    let mut written = 0usize;
    loop {
        let next = futures::future::poll_fn(|cx| connection.poll_next(cx)).await;
        let Some(outgoing) = next else {
            return CloseReason::Local;
        };
        let admitted = match outgoing.kind {
            FrameKind::Control | FrameKind::Data => true,
            FrameKind::Request(call_id) => admit_transmit(call_id, outgoing.bytes.len()),
        };
        if admitted && let Err(error) = writer.write_all(&outgoing.bytes).await {
            return CloseReason::Io(error.to_string());
        }
        // The frame left the queue: release what its admission reserved.
        drop(outgoing.release);
        // Flush whenever nothing else is ready, including after a skipped
        // frame, so earlier writes never sit in a buffer.
        if connection.is_drained() {
            if let Err(error) = writer.flush().await {
                return CloseReason::Io(error.to_string());
            }
            written = 0;
        } else {
            written += 1;
            if written >= batch.max(1) {
                written = 0;
                yield_now().await;
            }
        }
    }
}

/// Read and hand every complete frame payload to `on_frame` until the
/// stream ends or a frame is refused, yielding after every `batch` frames.
///
/// `on_frame` never waits: every frame is handled (queued, answered,
/// refused) synchronously, so a full application queue can never stall
/// the reader or the control frames behind it.
pub(crate) async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    max_frame_bytes: u32,
    chunk_bytes: usize,
    batch: usize,
    mut on_frame: impl FnMut(Vec<u8>) -> Result<(), CloseReason>,
) -> CloseReason {
    let mut decoder = crate::protocol::FrameDecoder::new(max_frame_bytes);
    let mut chunk = vec![0; chunk_bytes.max(1)];
    let mut handled = 0usize;
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
                    handled += 1;
                    if handled >= batch.max(1) {
                        handled = 0;
                        yield_now().await;
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
