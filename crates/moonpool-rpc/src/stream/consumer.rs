//! The consuming side of one reply stream: in-order intake, the bounded
//! buffer, consumption acknowledgements and the single terminal outcome.
//!
//! Acknowledgements follow `FoundationDB`'s
//! `NetNotifiedQueueWithAcknowledgements`: an item that arrives while the
//! application waits for it is handed straight to it, and one that arrives
//! while nobody waits is queued and popped later. Either way the item is
//! acknowledged only when the application's poll actually returns it:
//! a waiting poll that was abandoned (a dropped `next()` future, a lost
//! `select!` branch) takes nothing and earns nothing. Reading bytes off the
//! socket never earns credit.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use super::credit::{Intake, IntakeViolation};
use super::producer::StreamCore;
use crate::codec::CodecId;
use crate::error::{ErrorReason, Execution, RpcError};
use crate::stats::Counters;
use crate::transport::connection::Connection;

/// Where a consumer's acknowledgements go: fixed by the first frame of
/// the stream (the acknowledgement route handshake).
#[derive(Clone)]
pub(crate) enum AckRoute {
    /// Over the connection that delivered the stream, as `STREAM_ACK`.
    Remote(Weak<Connection>),
    /// Straight to a producer in this runtime.
    Local(Weak<StreamCore>),
}

/// One received item: its codec, body, accounted size, and whether a
/// waiting reader was handed it on arrival.
type Item = (CodecId, Vec<u8>, u64, bool);

/// What the consumer yields: an item's codec and body, the terminal error
/// once, then `None`.
pub(crate) type Next = Option<Result<(CodecId, Vec<u8>), RpcError>>;

struct ConsumerState {
    intake: Intake,
    items: VecDeque<Item>,
    /// The consumer polled with nothing to take and waits (it may have
    /// stopped waiting since: nothing is acknowledged until it takes).
    waiting: bool,
    waker: Option<Waker>,
    route: Option<AckRoute>,
    /// The terminal outcome, observable after every received item.
    terminal: Option<Result<(), RpcError>>,
    /// The terminal error was handed out (then the stream yields `None`).
    finished: bool,
}

/// A consumed stream's shared state (the handle and the runtime's pending
/// call both hold it).
pub(crate) struct ConsumerCore {
    call_id: u64,
    window: u64,
    state: Mutex<ConsumerState>,
    counters: Arc<Counters>,
}

/// What the runtime must do after an item or an end.
pub(crate) enum Intook {
    /// Accepted.
    Accepted,
    /// Refused: the stream ended with this protocol error, and the
    /// producer must be told to stop.
    Violation(IntakeViolation),
}

impl ConsumerCore {
    /// A consumer for the stream `call_id` announcing `window` bytes. The
    /// window was reserved from the runtime's buffer budget by the caller
    /// and is released when the core is dropped.
    pub(crate) fn new(call_id: u64, window: u64, counters: &Arc<Counters>) -> Arc<Self> {
        Arc::new(Self {
            call_id,
            window,
            state: Mutex::new(ConsumerState {
                intake: Intake::new(window),
                items: VecDeque::new(),
                waiting: false,
                waker: None,
                route: None,
                terminal: None,
                finished: false,
            }),
            counters: Arc::clone(counters),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ConsumerState> {
        self.state
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// The announced window.
    pub(crate) fn window(&self) -> u64 {
        self.window
    }

    /// Received and not yet consumed accounted bytes.
    pub(crate) fn buffered(&self) -> u64 {
        self.lock().intake.buffered()
    }

    /// Items received so far.
    pub(crate) fn received(&self) -> u64 {
        self.lock().intake.items()
    }

    /// Fix the acknowledgement route before any item (a local producer).
    pub(crate) fn set_route(&self, route: AckRoute) {
        let mut state = self.lock();
        if state.route.is_none() {
            state.route = Some(route);
        }
    }

    /// The acknowledgement route, once known.
    pub(crate) fn route(&self) -> Option<AckRoute> {
        self.lock().route.clone()
    }

    /// Accept item `sequence`, arriving over `route`.
    pub(crate) fn on_item(
        &self,
        route: &AckRoute,
        sequence: u64,
        codec: CodecId,
        body: Vec<u8>,
    ) -> Intook {
        let size = crate::protocol::stream_item_frame_len(body.len());
        let mut state = self.lock();
        if state.terminal.is_some() {
            // Late: the stream already ended on this side.
            return Intook::Accepted;
        }
        if state.route.is_none() {
            state.route = Some(route.clone());
        }
        if let Err(violation) = state.intake.accept(sequence, size) {
            Counters::bump(&self.counters.stream_violations);
            state.terminal = Some(Err(RpcError::new(
                ErrorReason::StreamProtocol(violation.to_string()),
                Execution::Executed,
            )));
            let waker = state.waker.take();
            drop(state);
            if let Some(waker) = waker {
                waker.wake();
            }
            return Intook::Violation(violation);
        }
        self.counters
            .stream_buffered_bytes
            .fetch_add(size, Ordering::Relaxed);
        // Handed to a reader waiting for it, or queued for a later pop;
        // acknowledged only once the reader takes it.
        let immediate = state.waiting && state.items.is_empty();
        state.waiting = false;
        state.items.push_back((codec, body, size, immediate));
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
        Intook::Accepted
    }

    /// The producer's end: `None` for a normal end, else its error. Checks
    /// the announced item count; every received item stays observable
    /// before the outcome.
    pub(crate) fn on_end(&self, items: u64, error: Option<RpcError>) -> Intook {
        let mut state = self.lock();
        let outcome = match state.intake.end(items) {
            Err(violation) => {
                Counters::bump(&self.counters.stream_violations);
                Err(violation)
            }
            Ok(()) => Ok(()),
        };
        let terminal = match (&outcome, error) {
            (Err(violation), _) => Err(RpcError::new(
                ErrorReason::StreamProtocol(violation.to_string()),
                Execution::Executed,
            )),
            (Ok(()), None) => Ok(()),
            (Ok(()), Some(error)) => Err(Self::weigh(&state, error)),
        };
        if state.terminal.is_none() {
            state.terminal = Some(terminal);
        }
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
        match outcome {
            Ok(()) => Intook::Accepted,
            Err(violation) => Intook::Violation(violation),
        }
    }

    /// Items that arrived prove the producer ran: sharpen what an error
    /// says about execution.
    fn weigh(state: &ConsumerState, error: RpcError) -> RpcError {
        if state.intake.items() > 0 && error.execution() != Execution::Executed {
            RpcError::new(error.reason().clone(), Execution::Executed)
        } else {
            error
        }
    }

    /// End the stream with `error` (a rejection, a disconnect, shutdown)
    /// unless it already ended. Received items stay observable first.
    pub(crate) fn terminate(&self, error: RpcError) {
        let mut state = self.lock();
        if state.terminal.is_some() {
            return;
        }
        let error = Self::weigh(&state, error);
        state.terminal = Some(Err(error));
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// What the terminal error proves about execution, once known.
    pub(crate) fn terminal_execution(&self) -> Option<Execution> {
        match &self.lock().terminal {
            Some(Err(error)) => Some(error.execution()),
            _ => None,
        }
    }

    /// Whether the terminal outcome is known.
    pub(crate) fn is_terminated(&self) -> bool {
        self.lock().terminal.is_some()
    }

    /// The next item, the terminal error once, then `None`.
    pub(crate) fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Next> {
        let mut state = self.lock();
        if let Some((codec, body, size, immediate)) = state.items.pop_front() {
            let consumed = state.intake.consume(size);
            let route = state.route.clone();
            state.waiting = false;
            drop(state);
            self.counters
                .stream_buffered_bytes
                .fetch_sub(size, Ordering::Relaxed);
            if let Some(route) = route {
                Counters::bump(if immediate {
                    &self.counters.stream_acks_immediate
                } else {
                    &self.counters.stream_acks_popped
                });
                self.acknowledge(&route, consumed);
            }
            return Poll::Ready(Some(Ok((codec, body))));
        }
        match &state.terminal {
            Some(Ok(())) => return Poll::Ready(None),
            Some(Err(error)) => {
                if state.finished {
                    return Poll::Ready(None);
                }
                let error = error.clone();
                state.finished = true;
                return Poll::Ready(Some(Err(error)));
            }
            None => {}
        }
        state.waiting = true;
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// End the stream on this side after an item that did not decode.
    pub(crate) fn fail_locally(&self, error: RpcError) {
        let mut state = self.lock();
        let dropped: u64 = state.items.iter().map(|(_, _, size, _)| size).sum();
        state.items.clear();
        state.terminal = Some(Err(error));
        state.finished = true;
        drop(state);
        self.counters
            .stream_buffered_bytes
            .fetch_sub(dropped, Ordering::Relaxed);
    }

    fn acknowledge(&self, route: &AckRoute, consumed: u64) {
        match route {
            AckRoute::Remote(connection) => {
                if let Some(connection) = connection.upgrade() {
                    connection.signal_ack(self.call_id, consumed);
                }
            }
            AckRoute::Local(producer) => {
                if let Some(producer) = producer.upgrade() {
                    // A consumer of this runtime never breaks the protocol.
                    let _ = producer.on_ack(consumed);
                }
            }
        }
    }
}

impl Drop for ConsumerCore {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            let buffered: u64 = state.items.iter().map(|(_, _, size, _)| size).sum();
            self.counters
                .stream_buffered_bytes
                .fetch_sub(buffered, Ordering::Relaxed);
        }
        self.counters
            .stream_window_reserved
            .fetch_sub(self.window, Ordering::Relaxed);
    }
}
