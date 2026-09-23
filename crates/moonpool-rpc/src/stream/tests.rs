//! The producer and consumer cores wired to each other in memory: credit
//! reservation races, first-come-first-served waiting, the two
//! acknowledgement paths, violations, cancellation and ends under
//! exhausted credit. No runtime, no sockets: every poll is explicit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use futures::task::ArcWake;

use super::SendError;
use super::consumer::{AckRoute, ConsumerCore, Intook};
use super::credit::AckViolation;
use super::producer::{AckEffect, CoreRoute, LocalStreamSink, StreamCore, Terminal};
use crate::codec::CodecId;
use crate::protocol::{WireError, stream_item_frame_len};
use crate::stats::Counters;

/// Counts its wakeups.
#[derive(Default)]
struct Flag(AtomicUsize);

impl ArcWake for Flag {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Flag {
    fn waker(self: &Arc<Self>) -> Waker {
        futures::task::waker(Arc::clone(self))
    }

    fn woken(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// Records what a producer emitted.
#[derive(Default)]
struct Sink {
    items: Mutex<Vec<(u64, usize)>>,
    ends: Mutex<Vec<(u64, Option<WireError>)>>,
}

impl LocalStreamSink for Sink {
    fn local_item(
        &self,
        _call_id: u64,
        _producer: &Arc<StreamCore>,
        sequence: u64,
        _codec: CodecId,
        body: Vec<u8>,
    ) {
        self.items
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push((sequence, body.len()));
    }

    fn local_end(&self, _call_id: u64, items: u64, error: Option<WireError>) {
        self.ends
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push((items, error));
    }
}

fn producer(window: u64) -> (Arc<Sink>, Arc<StreamCore>) {
    let sink = Arc::new(Sink::default());
    let route: Weak<dyn LocalStreamSink> = Arc::downgrade(&sink) as Weak<dyn LocalStreamSink>;
    let core = StreamCore::new(
        1,
        CoreRoute::Local(route),
        window,
        u64::MAX,
        &Arc::new(Counters::default()),
        Box::new(()),
    );
    (sink, core)
}

/// One pending send: its place in line and its item.
struct Pending {
    ticket: Option<u64>,
    item: Option<(CodecId, Vec<u8>)>,
    size: u64,
    flag: Arc<Flag>,
}

impl Pending {
    fn new(body_len: usize) -> Self {
        Self {
            ticket: None,
            item: Some((CodecId::PROST, vec![0; body_len])),
            size: stream_item_frame_len(body_len),
            flag: Arc::new(Flag::default()),
        }
    }

    fn poll(&mut self, core: &Arc<StreamCore>) -> Poll<Result<(), SendError>> {
        let waker = self.flag.waker();
        let mut cx = Context::from_waker(&waker);
        core.poll_send(&mut cx, &mut self.ticket, &mut self.item, self.size)
    }
}

#[test]
fn concurrent_sends_never_oversubscribe_and_wait_in_line() {
    let item = stream_item_frame_len(10);
    let (sink, core) = producer(2 * item);
    let mut first = Pending::new(10);
    let mut second = Pending::new(10);
    let mut third = Pending::new(10);
    let mut small_late = Pending::new(0);
    assert_eq!(first.poll(&core), Poll::Ready(Ok(())));
    assert_eq!(second.poll(&core), Poll::Ready(Ok(())));
    // The window is used up: the third waits, and a smaller item that
    // would fit arithmetically cannot jump the line either.
    assert!(third.poll(&core).is_pending());
    assert!(small_late.poll(&core).is_pending());
    assert_eq!(core.in_flight(), 2 * item);
    assert_eq!(core.on_ack(item), AckEffect::Advanced);
    // Only the head of the line is woken.
    assert_eq!(third.flag.woken(), 1);
    assert_eq!(small_late.flag.woken(), 0);
    assert!(
        small_late.poll(&core).is_pending(),
        "still behind the third"
    );
    assert_eq!(third.poll(&core), Poll::Ready(Ok(())));
    // The third's success passes the turn on.
    assert_eq!(small_late.flag.woken(), 1);
    assert!(small_late.poll(&core).is_pending(), "no credit yet");
    assert_eq!(core.on_ack(2 * item), AckEffect::Advanced);
    assert_eq!(small_late.poll(&core), Poll::Ready(Ok(())));
    let sequences: Vec<u64> = sink
        .items
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .iter()
        .map(|(sequence, _)| *sequence)
        .collect();
    assert_eq!(sequences, [0, 1, 2, 3], "emitted in reservation order");
}

#[test]
fn an_abandoned_waiter_gives_up_its_place() {
    let item = stream_item_frame_len(10);
    let (_sink, core) = producer(item);
    let mut first = Pending::new(10);
    let mut abandoned = Pending::new(10);
    let mut next = Pending::new(10);
    assert_eq!(first.poll(&core), Poll::Ready(Ok(())));
    assert!(abandoned.poll(&core).is_pending());
    assert!(next.poll(&core).is_pending());
    core.abandon_send(&mut abandoned.ticket);
    assert_eq!(next.flag.woken(), 1, "the next in line is woken");
    assert!(next.poll(&core).is_pending(), "still no credit");
    assert_eq!(core.on_ack(item), AckEffect::Advanced);
    assert_eq!(next.poll(&core), Poll::Ready(Ok(())));
}

#[test]
fn a_maximum_sized_item_fits_and_a_larger_one_is_refused_at_once() {
    let window = stream_item_frame_len(100);
    let (_sink, core) = producer(window);
    let mut too_large = Pending::new(101);
    assert_eq!(
        too_large.poll(&core),
        Poll::Ready(Err(SendError::TooLarge {
            size: window + 1,
            limit: window
        }))
    );
    let mut largest = Pending::new(100);
    assert_eq!(largest.poll(&core), Poll::Ready(Ok(())));
    assert_eq!(
        core.terminal(),
        None,
        "a refused item leaves the stream open"
    );
}

#[test]
fn the_end_needs_no_credit_and_wakes_every_waiter() {
    let item = stream_item_frame_len(10);
    let (sink, core) = producer(item);
    let mut first = Pending::new(10);
    let mut waiting = Pending::new(10);
    assert_eq!(first.poll(&core), Poll::Ready(Ok(())));
    assert!(waiting.poll(&core).is_pending());
    // Credit is exhausted, yet the failure goes out after the item.
    assert!(core.end(Some(WireError::StreamFailed { code: 9 })));
    assert_eq!(waiting.flag.woken(), 1);
    assert_eq!(waiting.poll(&core), Poll::Ready(Err(SendError::Ended)));
    assert_eq!(
        *sink
            .ends
            .lock()
            .expect("Mutex poisoned: prior task panicked"),
        [(1, Some(WireError::StreamFailed { code: 9 }))]
    );
    assert!(!core.end(None), "exactly one terminal state");
    assert_eq!(core.on_ack(item), AckEffect::Ignored);
}

#[test]
fn a_bad_acknowledgement_ends_the_stream_with_a_protocol_error() {
    let item = stream_item_frame_len(10);
    let (sink, core) = producer(4 * item);
    for _ in 0..2 {
        assert_eq!(Pending::new(10).poll(&core), Poll::Ready(Ok(())));
    }
    assert_eq!(core.on_ack(item), AckEffect::Advanced);
    assert_eq!(
        core.on_ack(item),
        AckEffect::Ignored,
        "a repeat is harmless"
    );
    assert_eq!(
        core.on_ack(item + 1),
        AckEffect::Violation(AckViolation::Misaligned { got: item + 1 })
    );
    assert_eq!(
        core.on_ack(0),
        AckEffect::Violation(AckViolation::Regressing {
            acked: item,
            got: 0
        })
    );
    assert!(core.end_for_violation());
    assert_eq!(core.terminal(), Some(Terminal::Protocol));
    assert_eq!(
        *sink
            .ends
            .lock()
            .expect("Mutex poisoned: prior task panicked"),
        [(2, Some(WireError::StreamProtocol))]
    );
}

#[test]
fn cancellation_wakes_a_producer_waiting_for_credit() {
    let item = stream_item_frame_len(10);
    let (sink, core) = producer(item);
    assert_eq!(Pending::new(10).poll(&core), Poll::Ready(Ok(())));
    let mut waiting = Pending::new(10);
    assert!(waiting.poll(&core).is_pending());
    assert!(core.cancel());
    assert_eq!(waiting.flag.woken(), 1);
    assert_eq!(waiting.poll(&core), Poll::Ready(Err(SendError::Cancelled)));
    assert!(!core.end(None), "a cancelled stream emits no end");
    assert!(
        sink.ends
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .is_empty()
    );
}

/// The consumer acknowledges an item handed to a waiting reader at once,
/// and a queued item only when it is taken.
#[test]
fn the_two_acknowledgement_paths_return_credit_on_consumption() {
    let item = stream_item_frame_len(10);
    let (_sink, core) = producer(2 * item);
    let counters = Arc::new(Counters::default());
    counters
        .stream_window_reserved
        .fetch_add(2 * item, Ordering::Relaxed);
    let consumer = ConsumerCore::new(1, 2 * item, &counters);
    let route = AckRoute::Local(Arc::downgrade(&core));
    let flag = Arc::new(Flag::default());
    let waker = flag.waker();
    let mut cx = Context::from_waker(&waker);

    // Nobody waits: the item is queued and not acknowledged.
    assert_eq!(Pending::new(10).poll(&core), Poll::Ready(Ok(())));
    assert!(matches!(
        consumer.on_item(&route, 0, CodecId::PROST, vec![0; 10]),
        Intook::Accepted
    ));
    assert_eq!(core.in_flight(), item, "bytes read earn no credit");
    assert!(matches!(
        consumer.poll_next(&mut cx),
        Poll::Ready(Some(Ok(_)))
    ));
    assert_eq!(core.in_flight(), 0, "taken from the queue: acknowledged");
    assert_eq!(counters.stream_acks_popped.load(Ordering::Relaxed), 1);

    // The reader waits: the next item is handed over and acknowledged on
    // arrival.
    assert!(consumer.poll_next(&mut cx).is_pending());
    assert_eq!(Pending::new(10).poll(&core), Poll::Ready(Ok(())));
    assert!(matches!(
        consumer.on_item(&route, 1, CodecId::PROST, vec![0; 10]),
        Intook::Accepted
    ));
    assert_eq!(core.in_flight(), 0);
    assert_eq!(counters.stream_acks_immediate.load(Ordering::Relaxed), 1);
    assert_eq!(flag.woken(), 1);
    assert!(matches!(
        consumer.poll_next(&mut cx),
        Poll::Ready(Some(Ok(_)))
    ));

    // Out of sequence: refused, never skipped, and the stream ends after
    // what was received.
    assert!(matches!(
        consumer.on_item(&route, 5, CodecId::PROST, vec![0; 10]),
        Intook::Violation(_)
    ));
    assert!(matches!(
        consumer.poll_next(&mut cx),
        Poll::Ready(Some(Err(error))) if matches!(error.reason(), crate::ErrorReason::StreamProtocol(_))
    ));
    assert!(matches!(consumer.poll_next(&mut cx), Poll::Ready(None)));
    drop(consumer);
    assert_eq!(counters.stream_window_reserved.load(Ordering::Relaxed), 0);
    assert_eq!(counters.stream_buffered_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn received_items_stay_observable_before_the_terminal_error() {
    let counters = Arc::new(Counters::default());
    let consumer = ConsumerCore::new(1, 1000, &counters);
    let (_sink, core) = producer(1000);
    let route = AckRoute::Local(Arc::downgrade(&core));
    for sequence in 0..3 {
        let _ = consumer.on_item(&route, sequence, CodecId::PROST, vec![1]);
    }
    consumer.terminate(crate::RpcError::new(
        crate::ErrorReason::Disconnected,
        crate::Execution::MaybeExecuted,
    ));
    let waker = Arc::new(Flag::default()).waker();
    let mut cx = Context::from_waker(&waker);
    for _ in 0..3 {
        assert!(matches!(
            consumer.poll_next(&mut cx),
            Poll::Ready(Some(Ok(_)))
        ));
    }
    // Items arrived, so the handler demonstrably ran.
    assert!(matches!(
        consumer.poll_next(&mut cx),
        Poll::Ready(Some(Err(error))) if error.execution() == crate::Execution::Executed
    ));
    assert!(matches!(consumer.poll_next(&mut cx), Poll::Ready(None)));
    // An end announcing more items than arrived is a gap, not a success.
    let short = ConsumerCore::new(2, 1000, &counters);
    let _ = short.on_item(&route, 0, CodecId::PROST, vec![1]);
    assert!(matches!(short.on_end(2, None), Intook::Violation(_)));
}
