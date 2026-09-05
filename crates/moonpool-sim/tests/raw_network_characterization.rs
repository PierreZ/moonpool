//! Characterization tests for the raw simulated TCP contract.
//!
//! These tests intentionally exercise behavior at the provider/stream boundary.
//! They are compatibility tripwires for the separation between the global
//! scheduler and the network state machine.

use std::{
    future::Future,
    io,
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};

use futures::{
    future::join,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    task::noop_waker,
};
use moonpool_sim::{
    NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait, buggify_init, buggify_reset,
    reset_rng_call_count, rng_call_count,
};

const MAX_DRIVER_STEPS: usize = 100_000;

/// Poll a simulation-backed future, advancing virtual time whenever it parks.
fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);

    for _ in 0..MAX_DRIVER_STEPS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }

        assert!(
            sim.has_pending_events(),
            "simulation-backed future stalled without a pending event"
        );
        sim.step();
    }

    panic!("simulation-backed future exceeded {MAX_DRIVER_STEPS} events")
}

fn poll_write_once(stream: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> Poll<io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_write(&mut context, data)
}

fn poll_read_once(
    stream: &mut (impl AsyncRead + Unpin),
    data: &mut [u8],
) -> Poll<io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_read(&mut context, data)
}

fn poll_write_vectored_once(
    stream: &mut (impl AsyncWrite + Unpin),
    data: &[io::IoSlice<'_>],
) -> Poll<io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_write_vectored(&mut context, data)
}

fn fast_world(seed: u64) -> SimWorld {
    SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), seed)
}

#[test]
fn concurrent_connects_are_accepted_in_fifo_order() {
    buggify_reset();
    let mut sim = fast_world(11);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid loopback IP"));
    let listener = drive(&mut sim, provider.bind("fifo-listener")).expect("listener binds");

    // `join` polls the left connect before the right connect. The listener must
    // retain both server endpoints and expose them in that same FIFO order.
    let (first, second) = drive(
        &mut sim,
        join(
            provider.connect("fifo-listener"),
            provider.connect("fifo-listener"),
        ),
    );
    let mut first = first.expect("first client connects");
    let mut second = second.expect("second client connects");

    drive(&mut sim, first.write_all(b"first")).expect("first client writes");
    drive(&mut sim, second.write_all(b"second")).expect("second client writes");

    let (mut first_server, _) =
        drive(&mut sim, listener.accept()).expect("first connection is queued");
    let (mut second_server, _) =
        drive(&mut sim, listener.accept()).expect("second connection is queued");

    sim.run_until_empty();

    let mut first_payload = [0; 5];
    drive(&mut sim, first_server.read_exact(&mut first_payload)).expect("first payload arrives");
    let mut second_payload = [0; 6];
    drive(&mut sim, second_server.read_exact(&mut second_payload)).expect("second payload arrives");

    assert_eq!(&first_payload, b"first");
    assert_eq!(&second_payload, b"second");
}

#[test]
fn scalar_and_vectored_writes_share_partial_backpressure_semantics() {
    buggify_reset();
    let mut sim = fast_world(12);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid loopback IP"));
    let listener = drive(&mut sim, provider.bind("backpressure-listener")).expect("listener binds");

    let mut scalar =
        drive(&mut sim, provider.connect("backpressure-listener")).expect("scalar client connects");
    let mut vectored = drive(&mut sim, provider.connect("backpressure-listener"))
        .expect("vectored client connects");
    let (_scalar_server, _) = drive(&mut sim, listener.accept()).expect("accept scalar client");
    let (_vectored_server, _) = drive(&mut sim, listener.accept()).expect("accept vectored client");

    let capacity = sim.available_send_buffer(scalar.connection_id());
    assert!(capacity > 0, "a fresh connection has send capacity");
    assert_eq!(
        sim.available_send_buffer(vectored.connection_id()),
        capacity,
        "fresh connections use the same send capacity"
    );

    let payload = vec![0x5a; capacity + 17];
    let scalar_accepted =
        poll_write_once(&mut scalar, &payload).map(|result| result.expect("scalar write succeeds"));
    let slices = [
        io::IoSlice::new(&payload[..capacity / 2]),
        io::IoSlice::new(&payload[capacity / 2..]),
    ];
    let vectored_accepted = poll_write_vectored_once(&mut vectored, &slices)
        .map(|result| result.expect("vectored write succeeds"));

    assert_eq!(scalar_accepted, Poll::Ready(capacity));
    assert_eq!(vectored_accepted, Poll::Ready(capacity));
    assert_eq!(sim.available_send_buffer(scalar.connection_id()), 0);
    assert_eq!(sim.available_send_buffer(vectored.connection_id()), 0);
    assert!(poll_write_once(&mut scalar, b"x").is_pending());
    assert!(poll_write_once(&mut vectored, b"x").is_pending());

    // Processing the queued sends releases capacity for both forms.
    sim.run_until_empty();
    assert!(sim.available_send_buffer(scalar.connection_id()) > 0);
    assert!(sim.available_send_buffer(vectored.connection_id()) > 0);
}

#[test]
fn no_progress_io_polls_do_not_consume_simulation_rng() {
    buggify_reset();
    let mut sim = fast_world(12_345);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid loopback IP"));
    let listener = drive(&mut sim, provider.bind("poll-entropy-listener")).expect("listener binds");
    let mut client =
        drive(&mut sim, provider.connect("poll-entropy-listener")).expect("client connects");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("server accepts");

    let capacity = sim.available_send_buffer(client.connection_id());
    let payload = vec![0x5a; capacity];
    let accepted = poll_write_once(&mut client, &payload)
        .map(|result| result.expect("initial write succeeds"));
    assert_eq!(accepted, Poll::Ready(capacity));
    assert_eq!(sim.available_send_buffer(client.connection_id()), 0);

    let mut chaos = NetworkConfiguration::fast_local();
    chaos.chaos.random_close_probability = 0.5;
    chaos.chaos.random_close_cooldown = Duration::ZERO;
    chaos.chaos.clog_probability = 0.5;
    sim.set_network_config(chaos);
    // Keep random-close call sites enabled but inactive. Their first encounter
    // would still consume an activation draw, making this a sensitive check
    // that neither pending path reaches a random decision.
    buggify_init(0.0);
    reset_rng_call_count();

    let mut byte = [0_u8; 1];
    for _ in 0..3 {
        assert!(poll_write_once(&mut client, b"x").is_pending());
        assert!(poll_read_once(&mut server, &mut byte).is_pending());
    }

    assert_eq!(
        rng_call_count(),
        0,
        "spurious backpressure and no-data polls must be entropy-neutral"
    );
    buggify_reset();
}

#[test]
fn graceful_close_delivers_buffered_bytes_before_fin() {
    buggify_reset();
    let mut sim = fast_world(13);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid loopback IP"));
    let listener = drive(&mut sim, provider.bind("fin-listener")).expect("listener binds");
    let mut client = drive(&mut sim, provider.connect("fin-listener")).expect("client connects");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("server accepts");

    drive(&mut sim, client.write_all(b"first-")).expect("first write succeeds");
    drive(&mut sim, client.write_all(b"second")).expect("second write succeeds");
    drive(&mut sim, client.close()).expect("graceful close succeeds");
    sim.run_until_empty();

    let mut payload = [0; 12];
    drive(&mut sim, server.read_exact(&mut payload)).expect("buffered bytes precede FIN");
    assert_eq!(&payload, b"first-second");

    let mut byte = [0; 1];
    let read = drive(&mut sim, server.read(&mut byte)).expect("read after FIN succeeds");
    assert_eq!(read, 0, "FIN becomes EOF after the receive buffer drains");
}

#[test]
fn graceful_close_orders_fin_after_an_already_scheduled_delivery() {
    buggify_reset();
    let mut sim = fast_world(7);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid loopback IP"));
    let listener =
        drive(&mut sim, provider.bind("scheduled-fin-listener")).expect("listener binds");
    let mut client =
        drive(&mut sim, provider.connect("scheduled-fin-listener")).expect("client connects");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("server accepts");

    drive(&mut sim, client.write_all(b"scheduled")).expect("write succeeds");
    assert!(sim.step(), "the buffered send is processed");
    drive(&mut sim, client.close()).expect("graceful close succeeds");
    sim.run_until_empty();

    let mut payload = [0; 9];
    drive(&mut sim, server.read_exact(&mut payload)).expect("scheduled bytes precede FIN");
    assert_eq!(&payload, b"scheduled");

    let mut byte = [0; 1];
    let read = drive(&mut sim, server.read(&mut byte)).expect("read after FIN succeeds");
    assert_eq!(read, 0, "FIN becomes EOF after the scheduled delivery");
}

#[test]
fn aborted_close_is_reported_as_connection_reset() {
    buggify_reset();
    let mut sim = fast_world(14);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid loopback IP"));
    let listener = drive(&mut sim, provider.bind("rst-listener")).expect("listener binds");
    let client = drive(&mut sim, provider.connect("rst-listener")).expect("client connects");
    let (mut server, _) = drive(&mut sim, listener.accept()).expect("server accepts");

    sim.close_connection_abort(client.connection_id());

    let mut byte = [0; 1];
    let error = drive(&mut sim, server.read(&mut byte)).expect_err("RST must fail reads");
    assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
}

#[derive(Debug, PartialEq, Eq)]
struct ReplayTrace {
    events: Vec<(Duration, String)>,
    faults: Vec<(u64, String)>,
}

fn fault_replay(seed: u64) -> ReplayTrace {
    buggify_reset();
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.bit_flip_probability = 1.0;
    config.chaos.bit_flip_cooldown = Duration::ZERO;

    let mut sim = SimWorld::new_with_network_config_and_seed(config, seed);
    buggify_init(1.0);
    let provider = sim.network_provider("127.0.0.1".parse().expect("valid loopback IP"));
    let listener = drive(&mut sim, provider.bind("replay-listener")).expect("listener binds");
    let mut client = drive(&mut sim, provider.connect("replay-listener")).expect("client connects");
    let (_server, _) = drive(&mut sim, listener.accept()).expect("server accepts");

    for index in 0..8u8 {
        let payload = vec![index; 32];
        drive(&mut sim, client.write_all(&payload)).expect("payload is buffered");
    }

    let mut events = Vec::new();
    let mut faults = Vec::new();
    while sim.has_pending_events() {
        sim.step();
        events.push((
            sim.current_time(),
            format!("{:?}", sim.last_processed_event()),
        ));
        faults.extend(sim.take_faults().into_iter().map(|record| {
            let serialized = serde_json::to_string(&record.event).expect("fault serializes");
            (record.time_ms, serialized)
        }));
    }
    buggify_reset();

    ReplayTrace { events, faults }
}

#[test]
fn same_seed_replays_network_events_and_faults_exactly() {
    let first = fault_replay(0x5eed);
    let second = fault_replay(0x5eed);

    assert_eq!(
        first, second,
        "same seed must replay the raw network exactly"
    );
    assert!(
        first
            .faults
            .iter()
            .any(|(_, event)| event.contains("BitFlip")),
        "the replay must include an injected network fault"
    );
}
