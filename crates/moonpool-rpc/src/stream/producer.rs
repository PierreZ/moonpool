//! The producing side of one reply stream: credit, readiness, ordered
//! emission and the single terminal transition.
//!
//! Locks: `emit` (outer) serialises emissions so frames leave in sequence
//! order; `state` (inner) guards credit and waiters. Acknowledgements and
//! cancellations take only `state`, so a consumer in the same runtime may
//! acknowledge from inside an emission. With a remote route the frame is
//! queued on the connection while `state` is held (order: stream state,
//! then connection); nothing ever takes a stream's lock while holding a
//! connection's.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use super::SendError;
use super::credit::{AckOutcome, AckViolation, Credit, Refusal};
use crate::codec::CodecId;
use crate::protocol::{WireError, WireMessage, encode_frame, encode_message};
use crate::stats::Counters;
use crate::transport::connection::{Connection, Release, StreamFrame};

/// Why a stream stopped producing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Terminal {
    /// The producer ended it (normally, with an error, or by dropping it).
    Ended,
    /// The consumer abandoned it.
    Cancelled,
    /// The connection to the consumer ended.
    Disconnected,
    /// The runtime shut down.
    Shutdown,
    /// The consumer broke the protocol; the stream was ended with an error.
    Protocol,
}

impl Terminal {
    fn error(self) -> SendError {
        match self {
            Self::Ended => SendError::Ended,
            Self::Cancelled => SendError::Cancelled,
            Self::Disconnected => SendError::Disconnected,
            Self::Shutdown => SendError::Shutdown,
            Self::Protocol => SendError::Protocol("the consumer broke the stream protocol".into()),
        }
    }
}

/// Delivers a stream produced for a caller in the same runtime.
pub(crate) trait LocalStreamSink: Send + Sync {
    /// Item `sequence` of the local stream `call_id`.
    fn local_item(
        &self,
        call_id: u64,
        producer: &Arc<StreamCore>,
        sequence: u64,
        codec: CodecId,
        body: Vec<u8>,
    );
    /// The end of the local stream `call_id`.
    fn local_end(&self, call_id: u64, items: u64, error: Option<WireError>);
}

/// Where a stream's frames go.
pub(crate) enum CoreRoute {
    /// Over the connection the request arrived on.
    Remote(Weak<Connection>),
    /// To a caller in this runtime.
    Local(Weak<dyn LocalStreamSink>),
}

/// A stream slot of the runtime's stream budget, held until the stream
/// ends.
pub(crate) struct StreamSlot(Arc<Counters>);

impl StreamSlot {
    /// Take a slot if fewer than `max` streams are produced.
    pub(crate) fn acquire(counters: &Arc<Counters>, max: usize) -> Option<Self> {
        let before = counters.live_server_streams.fetch_add(1, Ordering::Relaxed);
        if before >= max {
            counters.live_server_streams.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(Self(Arc::clone(counters)))
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.live_server_streams.fetch_sub(1, Ordering::Relaxed);
    }
}

/// An acknowledgement's effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AckEffect {
    /// Credit returned.
    Advanced,
    /// Ignored: a repeat of the last value, or the stream already ended.
    Ignored,
    /// Refused: the stream must be ended with a protocol error.
    Violation(AckViolation),
}

struct Waiter {
    ticket: u64,
    waker: Waker,
}

struct CoreState {
    credit: Credit,
    terminal: Option<Terminal>,
    /// Senders waiting for credit, served first come first served.
    waiters: VecDeque<Waiter>,
    /// `ready()` callers waiting for free credit.
    ready_waiters: Vec<Waker>,
    next_ticket: u64,
    /// What the stream holds until it ends: the room its admission
    /// reserved and its stream slot. Moved into the end frame, so it is
    /// released once the end has been written.
    hold: Option<Release>,
}

impl CoreState {
    /// Drop `ticket` from the waiters (the send finished or was abandoned).
    fn leave(&mut self, ticket: &mut Option<u64>) {
        if let Some(ticket) = ticket.take() {
            self.waiters.retain(|waiter| waiter.ticket != ticket);
        }
    }

    /// The waker of the sender now at the head of the line.
    fn head(&self) -> Option<Waker> {
        self.waiters.front().map(|waiter| waiter.waker.clone())
    }

    /// Every waiting waker (the stream ended).
    fn all(&mut self) -> Vec<Waker> {
        let mut wakers: Vec<Waker> = self.waiters.drain(..).map(|waiter| waiter.waker).collect();
        wakers.append(&mut self.ready_waiters);
        wakers
    }
}

fn wake(wakers: impl IntoIterator<Item = Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

/// The shared state of one produced stream.
pub(crate) struct StreamCore {
    call_id: u64,
    route: CoreRoute,
    emit: Mutex<()>,
    state: Mutex<CoreState>,
    counters: Arc<Counters>,
    /// The largest accounted item size the session can carry.
    frame_cap: u64,
}

impl StreamCore {
    pub(crate) fn new(
        call_id: u64,
        route: CoreRoute,
        window: u64,
        frame_cap: u64,
        counters: &Arc<Counters>,
        hold: Release,
    ) -> Arc<Self> {
        Arc::new(Self {
            call_id,
            route,
            emit: Mutex::new(()),
            state: Mutex::new(CoreState {
                credit: Credit::new(window),
                terminal: None,
                waiters: VecDeque::new(),
                ready_waiters: Vec::new(),
                next_ticket: 0,
                hold: Some(hold),
            }),
            counters: Arc::clone(counters),
            frame_cap,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CoreState> {
        self.state
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    fn emission(&self) -> std::sync::MutexGuard<'_, ()> {
        self.emit
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// The effective window.
    pub(crate) fn window(&self) -> u64 {
        self.lock().credit.window()
    }

    /// Lower the window (`FoundationDB`'s `setByteLimit`).
    pub(crate) fn limit_window(&self, limit: u64) {
        self.lock().credit.limit_window(limit);
    }

    /// Accounted bytes sent and not yet acknowledged.
    pub(crate) fn in_flight(&self) -> u64 {
        self.lock().credit.in_flight()
    }

    /// Items sent so far.
    pub(crate) fn items(&self) -> u64 {
        self.lock().credit.items()
    }

    /// How the stream ended, if it did.
    pub(crate) fn terminal(&self) -> Option<Terminal> {
        self.lock().terminal
    }

    /// The limit an item of `size` bytes is judged against.
    fn cap(&self, state: &CoreState) -> u64 {
        state.credit.window().min(self.frame_cap)
    }

    /// Try to send one item: reserve its credit (first come, first served)
    /// and queue it, or register to be woken. `ticket` is this send's place
    /// in line, `item` its encoded body (taken once sent).
    pub(crate) fn poll_send(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        ticket: &mut Option<u64>,
        item: &mut Option<(CodecId, Vec<u8>)>,
        size: u64,
    ) -> Poll<Result<(), SendError>> {
        let _emit = self.emission();
        let mut state = self.lock();
        if let Some(terminal) = state.terminal {
            state.leave(ticket);
            return Poll::Ready(Err(terminal.error()));
        }
        let cap = self.cap(&state);
        if size > cap {
            state.leave(ticket);
            let head = state.head();
            drop(state);
            wake(head);
            Counters::bump(&self.counters.stream_items_refused);
            return Poll::Ready(Err(SendError::TooLarge { size, limit: cap }));
        }
        let my_turn = match (state.waiters.front(), *ticket) {
            (None, _) => true,
            (Some(head), Some(mine)) => head.ticket == mine,
            (Some(_), None) => false,
        };
        if my_turn {
            match state.credit.reserve(size) {
                Ok(sequence) => {
                    state.leave(ticket);
                    let head = state.head();
                    let Some((codec, body)) = item.take() else {
                        return Poll::Ready(Err(SendError::Ended));
                    };
                    let sent = self.emit_item(state, sequence, codec, body);
                    wake(head);
                    return Poll::Ready(sent);
                }
                Err(Refusal::TooLarge { size, limit }) => {
                    state.leave(ticket);
                    return Poll::Ready(Err(SendError::TooLarge { size, limit }));
                }
                Err(Refusal::Overflow) => {
                    state.leave(ticket);
                    self.end_with(state, Terminal::Protocol, Some(WireError::StreamProtocol));
                    return Poll::Ready(Err(SendError::Protocol(
                        "stream byte counter overflow".into(),
                    )));
                }
                Err(Refusal::Wait) => {}
            }
        }
        if let Some(mine) = *ticket {
            if let Some(waiter) = state
                .waiters
                .iter_mut()
                .find(|waiter| waiter.ticket == mine)
            {
                waiter.waker.clone_from(cx.waker());
            }
        } else {
            let mine = state.next_ticket;
            state.next_ticket += 1;
            state.waiters.push_back(Waiter {
                ticket: mine,
                waker: cx.waker().clone(),
            });
            *ticket = Some(mine);
            Counters::bump(&self.counters.stream_credit_waits);
        }
        Poll::Pending
    }

    /// Queue an item whose credit was just reserved (`state` still held).
    fn emit_item(
        self: &Arc<Self>,
        state: std::sync::MutexGuard<'_, CoreState>,
        sequence: u64,
        codec: CodecId,
        body: Vec<u8>,
    ) -> Result<(), SendError> {
        match &self.route {
            CoreRoute::Remote(connection) => {
                let payload = encode_message(&WireMessage::StreamItem {
                    call_id: self.call_id,
                    sequence,
                    codec,
                    body,
                });
                // Already bounded by the session frame limit (`frame_cap`).
                let bytes = encode_frame(&payload, u32::MAX).unwrap_or_default();
                let queued = connection.upgrade().is_some_and(|connection| {
                    connection.push_stream_frame(
                        self.call_id,
                        StreamFrame {
                            bytes,
                            release: None,
                        },
                    )
                });
                drop(state);
                if queued {
                    Counters::bump(&self.counters.stream_items_sent);
                    Ok(())
                } else {
                    self.terminate(Terminal::Disconnected);
                    Err(SendError::Disconnected)
                }
            }
            CoreRoute::Local(sink) => {
                // Released first: a local consumer may acknowledge at once.
                drop(state);
                let Some(sink) = sink.upgrade() else {
                    self.terminate(Terminal::Shutdown);
                    return Err(SendError::Shutdown);
                };
                sink.local_item(self.call_id, self, sequence, codec, body);
                Counters::bump(&self.counters.stream_items_sent);
                Ok(())
            }
        }
    }

    /// Ready once any credit is free and no sender waits (`FoundationDB`'s
    /// `onReady`), or with the error that ended the stream.
    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), SendError>> {
        let mut state = self.lock();
        if let Some(terminal) = state.terminal {
            return Poll::Ready(Err(terminal.error()));
        }
        if state.credit.has_room() && state.waiters.is_empty() {
            return Poll::Ready(Ok(()));
        }
        if !state
            .ready_waiters
            .iter()
            .any(|waker| waker.will_wake(cx.waker()))
        {
            state.ready_waiters.push(cx.waker().clone());
        }
        Poll::Pending
    }

    /// A send future was dropped while waiting: give up its place.
    pub(crate) fn abandon_send(&self, ticket: &mut Option<u64>) {
        if ticket.is_none() {
            return;
        }
        let mut state = self.lock();
        state.leave(ticket);
        let head = state.head();
        drop(state);
        wake(head);
    }

    /// End the stream: `error` `None` is a normal end. Emitted after every
    /// item already queued; returns whether an end was emitted (`false`
    /// once the stream had already ended).
    pub(crate) fn end(self: &Arc<Self>, error: Option<WireError>) -> bool {
        self.end_locked(Terminal::Ended, error)
    }

    /// The consumer broke the protocol: end the stream with an error.
    pub(crate) fn end_for_violation(self: &Arc<Self>) -> bool {
        self.end_locked(Terminal::Protocol, Some(WireError::StreamProtocol))
    }

    fn end_locked(self: &Arc<Self>, terminal: Terminal, error: Option<WireError>) -> bool {
        let _emit = self.emission();
        let state = self.lock();
        self.end_with(state, terminal, error)
    }

    /// End with the emission lock held by the caller.
    fn end_with(
        self: &Arc<Self>,
        mut state: std::sync::MutexGuard<'_, CoreState>,
        terminal: Terminal,
        error: Option<WireError>,
    ) -> bool {
        if state.terminal.is_some() {
            return false;
        }
        state.terminal = Some(terminal);
        let items = state.credit.items();
        let hold = state.hold.take();
        let wakers = state.all();
        match &self.route {
            CoreRoute::Remote(connection) => {
                let payload = encode_message(&WireMessage::StreamEnd {
                    call_id: self.call_id,
                    items,
                    error,
                });
                let bytes = encode_frame(&payload, u32::MAX).unwrap_or_default();
                if let Some(connection) = connection.upgrade() {
                    // Forgotten at once: a late acknowledgement or cancel is
                    // ignored; the end still follows every queued item.
                    let _ = connection.remove_stream(self.call_id, false);
                    let _ = connection.push_stream_frame(
                        self.call_id,
                        StreamFrame {
                            bytes,
                            release: hold,
                        },
                    );
                }
                drop(state);
            }
            CoreRoute::Local(sink) => {
                drop(state);
                if let Some(sink) = sink.upgrade() {
                    sink.local_end(self.call_id, items, error);
                }
                drop(hold);
            }
        }
        Counters::bump(&self.counters.streams_ended);
        wake(wakers);
        true
    }

    /// Apply the consumer's cumulative acknowledgement.
    pub(crate) fn on_ack(&self, consumed: u64) -> AckEffect {
        let mut state = self.lock();
        if state.terminal.is_some() {
            return AckEffect::Ignored;
        }
        match state.credit.ack(consumed) {
            Ok(AckOutcome::Advanced) => {
                let mut wakers: Vec<Waker> = state.head().into_iter().collect();
                wakers.append(&mut state.ready_waiters);
                drop(state);
                wake(wakers);
                AckEffect::Advanced
            }
            Ok(AckOutcome::Duplicate) => AckEffect::Ignored,
            Err(violation) => AckEffect::Violation(violation),
        }
    }

    /// The consumer abandoned the stream: stop, drop what was not written,
    /// release everything. Returns whether it was still running.
    pub(crate) fn cancel(&self) -> bool {
        let mut state = self.lock();
        if state.terminal.is_some() {
            return false;
        }
        state.terminal = Some(Terminal::Cancelled);
        let hold = state.hold.take();
        let wakers = state.all();
        let discarded = match &self.route {
            CoreRoute::Remote(connection) => connection
                .upgrade()
                .map(|connection| connection.remove_stream(self.call_id, true))
                .unwrap_or_default(),
            CoreRoute::Local(_) => Vec::new(),
        };
        drop(state);
        drop(discarded);
        drop(hold);
        wake(wakers);
        true
    }

    /// The route is gone (connection ended, runtime shut down): stop and
    /// release everything; nothing more can be emitted.
    pub(crate) fn terminate(&self, terminal: Terminal) {
        let mut state = self.lock();
        if state.terminal.is_some() {
            return;
        }
        state.terminal = Some(terminal);
        let hold = state.hold.take();
        let wakers = state.all();
        drop(state);
        drop(hold);
        wake(wakers);
    }
}
