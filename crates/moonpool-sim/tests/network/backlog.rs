//! A listener's pending and reserved accept slots are bounded. Parked
//! connects resume in poll order and belong to one listener generation.

use crate::sync_drive::{drive, settle};
use futures::task::noop_waker;
use moonpool_sim::{
    LatencyDistribution, NetworkConfiguration, NetworkProvider, SimWorld, SimulationBuilder,
    TcpListenerTrait, TimeProvider, buggify_reset,
};
use std::{
    future::Future,
    io,
    net::IpAddr,
    pin::Pin,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

const SERVER: &str = "10.0.1.2:8080";

fn ip(value: &str) -> IpAddr {
    value.parse().expect("valid test IP")
}

fn world(capacity: usize) -> SimWorld {
    buggify_reset();
    let mut config = NetworkConfiguration::fast_local();
    config.accept_backlog_capacity = capacity;
    SimWorld::new_with_network_config_and_seed(config, 20_260_915)
}

fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = noop_waker();
    future.as_mut().poll(&mut Context::from_waker(&waker))
}

struct WakeCount(Arc<AtomicUsize>);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker(count: &Arc<AtomicUsize>) -> Waker {
    Waker::from(Arc::new(WakeCount(Arc::clone(count))))
}

fn poll_with<F: Future>(mut future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
    future.as_mut().poll(&mut Context::from_waker(waker))
}

#[test]
fn backlog_defaults_to_128_slots() {
    assert_eq!(NetworkConfiguration::default().accept_backlog_capacity, 128);
    assert_eq!(
        NetworkConfiguration::fast_local().accept_backlog_capacity,
        128
    );
}

#[test]
#[should_panic(expected = "accept backlog capacity must be positive")]
fn builder_rejects_a_zero_backlog_capacity() {
    let _ = SimulationBuilder::new().accept_backlog_capacity(0);
}

#[test]
fn full_backlog_parks_connect_until_accept_returns() {
    let mut sim = world(1);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("first connect");
    let mut second = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, second.as_mut()).is_pending());
    assert!(!sim.has_pending_events(), "a full backlog parks connect");

    let (accepted, _) = drive(&mut sim, listener.accept()).expect("first accept");
    let Poll::Ready(Ok(second)) = settle(&mut sim, second.as_mut()) else {
        panic!("second connect should resume");
    };
    let (accepted_second, _) = drive(&mut sim, listener.accept()).expect("second accept");
    drop((first, second, accepted, accepted_second));
}

#[test]
fn reserved_accept_counts_until_its_delayed_future_returns() {
    let mut sim = world(1);
    let mut config = sim.with_network_config(Clone::clone);
    config.accept_latency = LatencyDistribution::Uniform {
        start: Duration::from_mins(1),
        end: Duration::from_mins(1),
    };
    sim.set_network_config(config);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("first connect");
    let mut accepting = pin!(listener.accept());
    assert!(
        poll_once(accepting.as_mut()).is_pending(),
        "accept reserves endpoint"
    );
    let mut second = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, second.as_mut()).is_pending());
    assert!(
        !sim.has_pending_events(),
        "accept delay has elapsed without a repoll"
    );

    let Poll::Ready(Ok((accepted, _))) = poll_once(accepting.as_mut()) else {
        panic!("delayed accept should now return");
    };
    let Poll::Ready(Ok(second)) = settle(&mut sim, second.as_mut()) else {
        panic!("second connect should resume after return");
    };
    drop((first, accepted, second));
}

fn world_with_accept_latency(latency: Duration) -> SimWorld {
    let sim = world(8);
    let mut config = sim.with_network_config(Clone::clone);
    config.accept_latency = LatencyDistribution::Uniform {
        start: latency,
        end: latency,
    };
    let mut sim = sim;
    sim.set_network_config(config);
    sim
}

/// #210: the accept latency belongs to the connection, drawn when it enters
/// the backlog. An accept dropped mid-delay and re-created returns the
/// connection at the instant first scheduled, not a full latency later.
#[test]
fn a_dropped_accept_does_not_restart_the_connections_latency() {
    let latency = Duration::from_millis(10);
    let mut sim = world_with_accept_latency(latency);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let time = sim.time_provider();
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let client_stream = drive(&mut sim, client.connect(SERVER)).expect("connect");
    let queued_at = sim.current_time();

    {
        let mut first = pin!(listener.accept());
        assert!(poll_once(first.as_mut()).is_pending(), "accept reserves");
        let mut nap = pin!(time.sleep(Duration::from_millis(4)));
        assert!(settle(&mut sim, nap.as_mut()).is_ready());
        assert_eq!(sim.current_time(), queued_at + Duration::from_millis(4));
    }

    let (accepted, _) = drive(&mut sim, listener.accept()).expect("second accept");
    assert_eq!(
        sim.current_time(),
        queued_at + latency,
        "the replacement accept waits out only the remainder"
    );
    drop((client_stream, accepted));
}

/// #210: the `select!` idiom that re-creates its accept arm on every pass
/// must not starve while a sibling arm fires faster than the accept latency.
#[test]
fn an_accept_recreated_every_millisecond_still_completes_on_time() {
    let latency = Duration::from_millis(8);
    let mut sim = world_with_accept_latency(latency);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let time = sim.time_provider();
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let client_stream = drive(&mut sim, client.connect(SERVER)).expect("connect");
    let queued_at = sim.current_time();

    let mut accepted = None;
    for _pass in 0..100 {
        let mut accept = pin!(listener.accept());
        if let Poll::Ready(result) = poll_once(accept.as_mut()) {
            accepted = Some(result.expect("accept"));
            break;
        }
        // The sibling arm wins the pass; `select!` drops the accept arm.
        let mut tick = pin!(time.sleep(Duration::from_millis(1)));
        assert!(settle(&mut sim, tick.as_mut()).is_ready());
    }
    let (accepted, _) = accepted.expect("accept starved behind a 1 ms sibling arm");
    assert!(
        sim.current_time() <= queued_at + latency + Duration::from_millis(1),
        "accepted at {:?}, queued at {queued_at:?}",
        sim.current_time()
    );
    drop((client_stream, accepted));
}

#[test]
fn growing_capacity_wakes_multiple_parked_connects_after_queue_and_reservation_fill_it() {
    let mut sim = world(3);
    let mut config = sim.with_network_config(Clone::clone);
    config.accept_latency = LatencyDistribution::Uniform {
        start: Duration::from_mins(1),
        end: Duration::from_mins(1),
    };
    sim.set_network_config(config);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("first connect");
    let second = drive(&mut sim, client.connect(SERVER)).expect("second connect");
    let third = drive(&mut sim, client.connect(SERVER)).expect("third connect");
    let mut accepting = pin!(listener.accept());
    assert!(
        poll_once(accepting.as_mut()).is_pending(),
        "first endpoint reserved"
    );
    let mut fourth = pin!(client.connect(SERVER));
    let mut fifth = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, fourth.as_mut()).is_pending());
    assert!(settle(&mut sim, fifth.as_mut()).is_pending());
    assert!(
        !sim.has_pending_events(),
        "two queued plus one reserved fill all slots"
    );

    let fourth_wakes = Arc::new(AtomicUsize::new(0));
    let fifth_wakes = Arc::new(AtomicUsize::new(0));
    assert!(poll_with(fourth.as_mut(), &counting_waker(&fourth_wakes)).is_pending());
    assert!(poll_with(fifth.as_mut(), &counting_waker(&fifth_wakes)).is_pending());
    let mut expanded = sim.with_network_config(Clone::clone);
    expanded.accept_backlog_capacity = 5;
    sim.set_network_config(expanded);
    assert_eq!(
        fourth_wakes.load(Ordering::SeqCst),
        1,
        "oldest waiter wakes first"
    );
    assert_eq!(
        fifth_wakes.load(Ordering::SeqCst),
        0,
        "newer waiter still waits"
    );
    assert!(
        poll_with(fifth.as_mut(), &counting_waker(&fifth_wakes)).is_pending(),
        "newer waiter cannot overtake"
    );
    let Poll::Ready(Ok(fourth)) = poll_once(fourth.as_mut()) else {
        panic!("capacity growth must publish oldest connect");
    };
    assert!(
        fifth_wakes.load(Ordering::SeqCst) > 0,
        "first publication wakes next"
    );
    let Poll::Ready(Ok(fifth)) = poll_once(fifth.as_mut()) else {
        panic!("capacity growth must publish next connect");
    };

    // The raised capacity is now full: four queued endpoints plus the
    // delayed accept reservation. It frees a slot only when accept returns.
    let mut sixth = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, sixth.as_mut()).is_pending());
    let Poll::Ready(Ok((accepted_first, _))) = poll_once(accepting.as_mut()) else {
        panic!("reserved accept should return after delay");
    };
    let Poll::Ready(Ok(sixth)) = poll_once(sixth.as_mut()) else {
        panic!("accept return should release the fifth slot");
    };
    drop((first, second, third, fourth, fifth, sixth, accepted_first));
}

#[test]
fn parked_connects_resume_fifo_and_cancelled_one_leaves_no_endpoint() {
    let mut sim = world(1);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("first connect");
    let mut second = pin!(client.connect(SERVER));
    let mut third = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, second.as_mut()).is_pending());
    assert!(settle(&mut sim, third.as_mut()).is_pending());

    let (accepted_first, _) = drive(&mut sim, listener.accept()).expect("accept first");
    assert!(
        settle(&mut sim, third.as_mut()).is_pending(),
        "third remains behind second"
    );
    let Poll::Ready(Ok(second)) = settle(&mut sim, second.as_mut()) else {
        panic!("oldest connect should resume first");
    };
    let (accepted_second, _) = drive(&mut sim, listener.accept()).expect("accept second");
    let Poll::Ready(Ok(third)) = settle(&mut sim, third.as_mut()) else {
        panic!("third connect should resume next");
    };
    let (accepted_third, _) = drive(&mut sim, listener.accept()).expect("accept third");
    drop((
        first,
        second,
        third,
        accepted_first,
        accepted_second,
        accepted_third,
    ));

    // A cancellation while parked must remove both its waiter and its
    // unpublished pair, leaving the next connect able to take the slot.
    let first = drive(&mut sim, client.connect(SERVER)).expect("fill backlog again");
    let mut cancelled = Box::pin(client.connect(SERVER));
    let mut survivor = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, cancelled.as_mut()).is_pending());
    assert!(settle(&mut sim, survivor.as_mut()).is_pending());
    drop(cancelled);
    let (accepted_first, _) =
        drive(&mut sim, listener.accept()).expect("accept after cancellation");
    let Poll::Ready(Ok(survivor)) = settle(&mut sim, survivor.as_mut()) else {
        panic!("surviving connect should resume");
    };
    let (accepted_survivor, _) = drive(&mut sim, listener.accept()).expect("no ghost endpoint");
    drop((first, survivor, accepted_first, accepted_survivor));
}

#[test]
fn dropping_listener_fails_parked_connect_even_after_same_address_rebind() {
    let mut sim = world(1);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind old listener");
    let first = drive(&mut sim, client.connect(SERVER)).expect("fill old backlog");
    let mut old_connect = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, old_connect.as_mut()).is_pending());
    drop(listener);
    let replacement = drive(&mut sim, server.bind(SERVER)).expect("rebind address");
    assert!(matches!(
        poll_once(old_connect.as_mut()),
        Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused
    ));
    let fresh = drive(&mut sim, client.connect(SERVER)).expect("new listener serves fresh connect");
    let (accepted, _) = drive(&mut sim, replacement.accept()).expect("accept fresh endpoint");
    drop((first, fresh, accepted));
}

#[test]
fn aborting_parked_unpublished_pair_fails_connect_instead_of_publishing_it() {
    let mut sim = world(1);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("fill backlog");
    let mut parked = Box::pin(client.connect(SERVER));
    assert!(settle(&mut sim, parked.as_mut()).is_pending());
    let wake_count = Arc::new(AtomicUsize::new(0));
    assert!(poll_with(parked.as_mut(), &counting_waker(&wake_count)).is_pending());

    // Killing only the client leaves the server listener live. It aborts
    // both the queued endpoint and the unpublished pair held by connect.
    sim.abort_all_connections_for_ip(ip("10.0.1.1"));
    assert!(
        wake_count.load(Ordering::SeqCst) > 0,
        "aborted connect must wake"
    );
    assert!(matches!(
        poll_once(parked.as_mut()),
        Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::ConnectionAborted
    ));
    drop(parked);

    let restarted = sim.network_provider(ip("10.0.1.1"));
    let fresh = drive(&mut sim, restarted.connect(SERVER)).expect("new connect after restart");
    let (accepted, _) = drive(&mut sim, listener.accept()).expect("accept only fresh endpoint");
    drop((first, fresh, accepted));
}

#[test]
fn cancelled_accept_reservation_keeps_the_slot_occupied() {
    let mut sim = world(1);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("first connect");
    let mut accepting = Box::pin(listener.accept());
    assert!(poll_once(accepting.as_mut()).is_pending());
    drop(accepting);
    let mut second = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, second.as_mut()).is_pending());
    let (accepted, _) = drive(&mut sim, listener.accept()).expect("returned reservation accepts");
    let Poll::Ready(Ok(second)) = settle(&mut sim, second.as_mut()) else {
        panic!("second connect should resume after accept");
    };
    drop((first, accepted, second));
}

#[test]
fn aborting_a_queued_endpoint_frees_its_slot_without_accept() {
    let mut sim = world(1);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("fill backlog");
    let mut second = pin!(client.connect(SERVER));
    assert!(settle(&mut sim, second.as_mut()).is_pending());

    sim.close_connection_abort(first.connection_id());
    let Poll::Ready(Ok(second)) = settle(&mut sim, second.as_mut()) else {
        panic!("reset queued endpoint must release slot");
    };
    let (accepted, _) = drive(&mut sim, listener.accept()).expect("accept surviving endpoint");
    drop((first, second, accepted));
}

#[test]
fn aborting_a_reserved_endpoint_frees_slot_and_fails_accept_before_delay() {
    let mut sim = world(1);
    let mut config = sim.with_network_config(Clone::clone);
    config.accept_latency = LatencyDistribution::Uniform {
        start: Duration::from_mins(1),
        end: Duration::from_mins(1),
    };
    sim.set_network_config(config);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("fill backlog");
    let mut accepting = pin!(listener.accept());
    assert!(
        poll_once(accepting.as_mut()).is_pending(),
        "reserve first endpoint"
    );
    let mut second = pin!(client.connect(SERVER));
    assert!(poll_once(second.as_mut()).is_pending());
    assert!(sim.has_pending_events());
    sim.step(); // The connect delay is shorter than the one-minute accept delay.
    assert!(
        poll_once(second.as_mut()).is_pending(),
        "reservation still fills slot"
    );

    sim.close_connection_abort(first.connection_id());
    assert!(
        matches!(
            poll_once(accepting.as_mut()),
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::ConnectionAborted
        ),
        "reserved accept reports the reset before its delay elapses"
    );
    let Poll::Ready(Ok(second)) = poll_once(second.as_mut()) else {
        panic!("reset reserved endpoint must release slot");
    };
    drop((first, second));
}

#[test]
fn reserved_accept_reset_wakes_its_latest_waker() {
    let mut sim = world(1);
    let mut config = sim.with_network_config(Clone::clone);
    config.accept_latency = LatencyDistribution::Uniform {
        start: Duration::from_mins(1),
        end: Duration::from_mins(1),
    };
    sim.set_network_config(config);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");
    let first = drive(&mut sim, client.connect(SERVER)).expect("fill backlog");
    let mut accepting = pin!(listener.accept());
    let old_count = Arc::new(AtomicUsize::new(0));
    let new_count = Arc::new(AtomicUsize::new(0));
    assert!(poll_with(accepting.as_mut(), &counting_waker(&old_count)).is_pending());
    assert!(poll_with(accepting.as_mut(), &counting_waker(&new_count)).is_pending());

    sim.close_connection_abort(first.connection_id());
    assert_eq!(
        old_count.load(Ordering::SeqCst),
        0,
        "old task must not be woken"
    );
    assert_eq!(
        new_count.load(Ordering::SeqCst),
        1,
        "latest task sees reset promptly"
    );
    assert!(matches!(
        poll_once(accepting.as_mut()),
        Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::ConnectionAborted
    ));
}

#[test]
fn retained_old_listener_accept_cannot_use_a_replacement_generation() {
    let mut sim = world(1);
    let server = sim.network_provider(ip("10.0.1.2"));
    let client = sim.network_provider(ip("10.0.1.1"));
    let old_listener = drive(&mut sim, server.bind(SERVER)).expect("bind old listener");
    let mut waiting_on_old = pin!(old_listener.accept());
    assert!(poll_once(waiting_on_old.as_mut()).is_pending());

    // A process crash releases the registry entry, but customer code may
    // still hold the old listener object while a replacement binds.
    sim.abort_all_connections_for_ip(ip("10.0.1.2"));
    let replacement = drive(&mut sim, server.bind(SERVER)).expect("bind replacement");
    assert!(matches!(
        poll_once(waiting_on_old.as_mut()),
        Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::ConnectionAborted
    ));
    assert_eq!(
        drive(&mut sim, old_listener.accept())
            .err()
            .map(|error| error.kind()),
        Some(io::ErrorKind::ConnectionAborted),
        "a new accept on the retained old object also fails"
    );

    let fresh = drive(&mut sim, client.connect(SERVER)).expect("fresh connect");
    let (accepted, _) = drive(&mut sim, replacement.accept()).expect("replacement accept");
    drop((fresh, accepted));
}
