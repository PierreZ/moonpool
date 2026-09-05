//! Faults reach traffic that is already on the wire.
//!
//! Every test fixes the write latency so the moment a chunk would land is
//! known exactly, sends, waits until the chunk is in flight (it has left the
//! send queue and has a delivery time), and only *then* injects the fault.
//! The delivery time was sampled before the fault existed; the fault must
//! still decide the chunk's fate. The semantic under test is the one the
//! partition chapter documents: a cut **stalls** the stream and never punches
//! a hole in it, so a held chunk lands after the heal, in order, with the
//! flight time it still had left when it was frozen.

use futures::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    task::noop_waker,
};
use moonpool_sim::{
    LatencyDistribution, NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait,
    buggify_reset, network::sim::SimTcpStream, reset_rng_call_count, rng_call_count,
};
use std::{
    future::Future,
    io,
    net::IpAddr,
    pin::{Pin, pin},
    task::{Context, Poll},
    time::Duration,
};

/// Every chunk lands this long after it is put on the wire.
const LATENCY: Duration = Duration::from_millis(10);
/// How long a scripted partition stays up on its own.
const CUT: Duration = Duration::from_millis(200);
/// How many events a driven future may consume before it is declared stuck.
const MAX_DRIVER_STEPS: usize = 10_000;

fn client_ip() -> IpAddr {
    "10.0.1.1".parse().expect("valid test IP")
}

fn server_ip() -> IpAddr {
    "10.0.1.2".parse().expect("valid test IP")
}

fn fixed_latency_config() -> NetworkConfiguration {
    let mut config = NetworkConfiguration::fast_local();
    config.write_latency = LatencyDistribution::Uniform {
        start: LATENCY,
        end: LATENCY,
    };
    config
}

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

fn poll_close_once(stream: &mut (impl AsyncWrite + Unpin)) -> Poll<io::Result<()>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_close(&mut context)
}

/// An established, settled connection between `10.0.1.1` and `10.0.1.2`.
fn connected() -> (SimWorld, SimTcpStream, SimTcpStream) {
    buggify_reset();
    let mut sim = SimWorld::new_with_network_config_and_seed(fixed_latency_config(), 20_260_905);
    let server_provider = sim.network_provider(server_ip());
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(client_ip());
    let client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();
    assert_eq!(sim.pending_event_count(), 0, "the handshake has settled");
    (sim, client, server)
}

/// Write `data` and drain it onto the wire, so it is in flight with a known
/// delivery time and nothing left in the send queue.
fn put_on_the_wire(sim: &mut SimWorld, stream: &mut SimTcpStream, data: &[u8]) {
    let before = sim.in_flight_bytes(stream.connection_id());
    assert_eq!(
        poll_write_once(stream, data).map(|result| result.expect("write is accepted")),
        Poll::Ready(data.len())
    );
    assert!(sim.step(), "the engine drains the queued send");
    assert_eq!(sim.queued_send_bytes(stream.connection_id()), 0);
    assert_eq!(
        sim.in_flight_bytes(stream.connection_id()),
        before + data.len(),
        "the chunk is on the wire"
    );
}

fn assert_nothing_readable(sim: &SimWorld, stream: &mut SimTcpStream) {
    let mut byte = [0_u8; 1];
    assert!(
        poll_read_once(stream, &mut byte).is_pending(),
        "no byte and no EOF crosses the cut"
    );
    assert_eq!(sim.unread_bytes(stream.connection_id()), 0);
}

/// The central case: the chunk is in flight, its delivery time already
/// sampled, when the partition lands. It must not cross, and once the cut
/// heals it lands with exactly the flight time it had left.
#[test]
fn a_partition_after_the_send_holds_the_bytes_in_flight() {
    const PAYLOAD: &[u8] = b"response-already-on-the-wire";
    let (mut sim, mut client, mut server) = connected();
    let sent_at = sim.now();
    put_on_the_wire(&mut sim, &mut client, PAYLOAD);

    sim.partition_pair(client_ip(), server_ip(), CUT);
    assert!(sim.is_in_flight_held(client.connection_id()));

    // The original delivery time comes and goes.
    assert!(sim.step());
    assert_eq!(
        sim.now(),
        sent_at + LATENCY,
        "the stale delivery event fired"
    );
    assert!(sim.is_partitioned(client_ip(), server_ip()));
    assert_nothing_readable(&sim, &mut server);
    assert_eq!(sim.in_flight_bytes(client.connection_id()), PAYLOAD.len());

    // The cut expires: the flight thaws, but the bytes still need the
    // remaining flight time, so nothing has landed yet.
    assert!(sim.step());
    assert_eq!(sim.now(), sent_at + CUT);
    assert!(!sim.is_partitioned(client_ip(), server_ip()));
    assert!(!sim.is_in_flight_held(client.connection_id()));
    assert_nothing_readable(&sim, &mut server);

    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the payload lands after the heal");
    assert_eq!(received, PAYLOAD);
    assert_eq!(
        sim.now(),
        sent_at + LATENCY + CUT,
        "a cut of D delays a caught chunk by exactly D"
    );
}

/// A cut healed by hand releases the flight from that instant, not from the
/// deadline it was cut under.
#[test]
fn healing_by_hand_releases_the_held_flight_with_its_remaining_time() {
    const PAYLOAD: &[u8] = b"held-then-released";
    let (mut sim, mut client, mut server) = connected();
    put_on_the_wire(&mut sim, &mut client, PAYLOAD);
    sim.partition_pair(client_ip(), server_ip(), Duration::from_hours(1));
    assert!(sim.step(), "the stale delivery event fires under the cut");
    assert_nothing_readable(&sim, &mut server);
    let healed_at = sim.now();

    sim.restore_partition(client_ip(), server_ip());
    assert_nothing_readable(&sim, &mut server);

    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the payload lands");
    assert_eq!(received, PAYLOAD);
    assert_eq!(
        sim.now().saturating_sub(healed_at),
        LATENCY,
        "the chunk was frozen the instant it was sent, so it needs its whole flight"
    );
}

/// A directed cut holds one direction and leaves the other flowing.
#[test]
fn a_directional_partition_holds_only_the_cut_direction() {
    const PING: &[u8] = b"ping-towards-the-server";
    const PONG: &[u8] = b"pong-towards-the-client";
    let (mut sim, mut client, mut server) = connected();
    let sent_at = sim.now();
    put_on_the_wire(&mut sim, &mut client, PING);
    put_on_the_wire(&mut sim, &mut server, PONG);

    sim.partition_pair(client_ip(), server_ip(), CUT);
    assert!(sim.is_in_flight_held(client.connection_id()));
    assert!(!sim.is_in_flight_held(server.connection_id()));

    let mut towards_client = vec![0_u8; PONG.len()];
    drive(&mut sim, client.read_exact(&mut towards_client))
        .expect("the unaffected direction lands");
    assert_eq!(towards_client, PONG);
    assert_eq!(sim.now(), sent_at + LATENCY);
    assert_nothing_readable(&sim, &mut server);

    let mut towards_server = vec![0_u8; PING.len()];
    drive(&mut sim, server.read_exact(&mut towards_server))
        .expect("the cut direction lands after the heal");
    assert_eq!(towards_server, PING);
    assert_eq!(sim.now(), sent_at + LATENCY + CUT);
}

/// Send-wide and receive-wide cuts hold the flight like a pair cut does.
#[test]
fn send_and_recv_partitions_hold_the_flight_too() {
    const PAYLOAD: &[u8] = b"held-by-a-node-wide-cut";
    for cut in [
        |sim: &SimWorld| sim.partition_send_from(client_ip(), CUT),
        |sim: &SimWorld| sim.partition_recv_to(server_ip(), CUT),
    ] {
        let (mut sim, mut client, mut server) = connected();
        let sent_at = sim.now();
        put_on_the_wire(&mut sim, &mut client, PAYLOAD);
        cut(&sim);
        assert!(sim.is_in_flight_held(client.connection_id()));
        assert!(sim.step(), "the stale delivery event fires under the cut");
        assert_nothing_readable(&sim, &mut server);

        let mut received = vec![0_u8; PAYLOAD.len()];
        drive(&mut sim, server.read_exact(&mut received)).expect("the payload lands");
        assert_eq!(received, PAYLOAD);
        assert_eq!(sim.now(), sent_at + LATENCY + CUT);
    }
}

/// Chunks on both sides of the cut keep their order: two already in flight,
/// one queued behind the cut, all three land as written.
#[test]
fn healing_never_lets_a_later_chunk_overtake_an_earlier_one() {
    const FIRST: &[u8] = b"first-in-flight";
    const SECOND: &[u8] = b"second-in-flight";
    const THIRD: &[u8] = b"third-queued-behind-the-cut";
    let (mut sim, mut client, mut server) = connected();
    put_on_the_wire(&mut sim, &mut client, FIRST);
    put_on_the_wire(&mut sim, &mut client, SECOND);

    sim.partition_pair(client_ip(), server_ip(), CUT);
    assert_eq!(
        poll_write_once(&mut client, THIRD).map(|result| result.expect("write is accepted")),
        Poll::Ready(THIRD.len())
    );
    assert!(sim.step(), "the queued send stalls under the cut");
    assert_eq!(sim.queued_send_bytes(client.connection_id()), THIRD.len());
    assert_eq!(
        sim.in_flight_bytes(client.connection_id()),
        FIRST.len() + SECOND.len()
    );
    // Both stale delivery events fire under the cut.
    while sim.is_partitioned(client_ip(), server_ip()) {
        assert!(sim.step());
    }
    assert_nothing_readable(&sim, &mut server);

    let mut received = vec![0_u8; FIRST.len() + SECOND.len() + THIRD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("all three chunks land");
    assert_eq!(
        received,
        [FIRST, SECOND, THIRD].concat(),
        "the stream is delivered in the order it was written"
    );
}

/// A FIN on the wire is held with the data ahead of it: the peer sees neither
/// the bytes nor EOF through the cut, then both, in that order.
#[test]
fn an_in_flight_fin_does_not_cross_a_partition() {
    const PAYLOAD: &[u8] = b"last-bytes-before-the-fin";
    let (mut sim, mut client, mut server) = connected();
    put_on_the_wire(&mut sim, &mut client, PAYLOAD);
    assert!(matches!(poll_close_once(&mut client), Poll::Ready(Ok(()))));

    sim.partition_pair(client_ip(), server_ip(), CUT);
    while sim.is_partitioned(client_ip(), server_ip()) {
        assert!(sim.step());
    }
    assert_nothing_readable(&sim, &mut server);

    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the data lands first");
    assert_eq!(received, PAYLOAD);
    let mut byte = [0_u8; 1];
    let eof = drive(&mut sim, server.read(&mut byte)).expect("then the FIN");
    assert_eq!(eof, 0, "EOF follows the last byte, never precedes it");
}

/// A graceful close issued *under* the cut puts its FIN behind the held data.
#[test]
fn a_close_under_the_partition_queues_its_fin_behind_the_held_data() {
    const PAYLOAD: &[u8] = b"data-frozen-before-the-close";
    let (mut sim, mut client, mut server) = connected();
    put_on_the_wire(&mut sim, &mut client, PAYLOAD);
    sim.partition_pair(client_ip(), server_ip(), CUT);
    assert!(matches!(poll_close_once(&mut client), Poll::Ready(Ok(()))));
    while sim.is_partitioned(client_ip(), server_ip()) {
        assert!(sim.step());
    }
    assert_nothing_readable(&sim, &mut server);

    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the data lands first");
    assert_eq!(received, PAYLOAD);
    let mut byte = [0_u8; 1];
    assert_eq!(
        drive(&mut sim, server.read(&mut byte)).expect("then the FIN"),
        0
    );
}

/// Recovery mode heals the cut and re-drives the held flight without a
/// single new draw on the simulation stream.
#[test]
fn recovery_mode_releases_the_held_flight_without_drawing() {
    const PAYLOAD: &[u8] = b"released-by-the-recovery-boundary";
    let (mut sim, mut client, mut server) = connected();
    put_on_the_wire(&mut sim, &mut client, PAYLOAD);
    sim.partition_pair(client_ip(), server_ip(), Duration::from_hours(1));
    assert!(sim.step(), "the stale delivery event fires under the cut");
    assert_nothing_readable(&sim, &mut server);

    reset_rng_call_count();
    sim.enter_recovery_mode();
    assert!(!sim.is_partitioned(client_ip(), server_ip()));
    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the payload lands");
    assert_eq!(received, PAYLOAD);
    assert_eq!(
        rng_call_count(),
        0,
        "healing and re-driving the flight is entropy-neutral"
    );
}

/// A black hole opened while the chunk is in flight swallows it at landing.
#[test]
fn a_black_hole_opened_mid_flight_swallows_the_chunk() {
    const PAYLOAD: &[u8] = b"into-the-void-mid-flight";
    let (mut sim, mut client, mut server) = connected();
    put_on_the_wire(&mut sim, &mut client, PAYLOAD);
    sim.black_hole_connection(client.connection_id(), true, false);
    sim.run_until_empty();
    assert_eq!(sim.in_flight_bytes(client.connection_id()), 0);
    assert_nothing_readable(&sim, &mut server);
}
