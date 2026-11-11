//! The failures a peer sees, and when it sees them.
//!
//! A process death is not an instant reset of both ends: the dead side's
//! bytes on the wire still land, and its peer learns of the death only
//! through the network. A host that lives on sends an RST behind those
//! bytes; a host that lost power sends nothing, and answers the peer's
//! next segment with an RST. A random close that cuts a receive side is a
//! reset, never a clean EOF. A listener bound by a name is served by the
//! machine that bound it, so faults on that machine reach its connections.

use crate::poll_io::{poll_read_once, poll_write_once};
use crate::sync_drive::drive;
use futures::io::AsyncReadExt;
use moonpool_sim::{
    DeathNotice, LatencyDistribution, NetworkConfiguration, NetworkProvider, SimWorld,
    TcpListenerTrait, buggify_init, buggify_reset, network::sim::SimTcpStream,
    reset_rng_call_count, rng_call_count,
};
use std::{io, net::IpAddr, task::Poll, time::Duration};

/// Every chunk lands this long after it is put on the wire.
const LATENCY: Duration = Duration::from_millis(10);
/// How long a scripted partition stays up on its own.
const CUT: Duration = Duration::from_millis(200);

fn client_ip() -> IpAddr {
    "10.0.1.1".parse().expect("valid test IP")
}

fn server_ip() -> IpAddr {
    "10.0.1.2".parse().expect("valid test IP")
}

fn world() -> SimWorld {
    buggify_reset();
    let mut config = NetworkConfiguration::fast_local();
    config.write_latency = LatencyDistribution::Uniform {
        start: LATENCY,
        end: LATENCY,
    };
    SimWorld::new_with_network_config_and_seed(config, 20_261_110)
}

/// An established, settled connection to a listener bound at `addr` by the
/// server machine.
fn connected_at(addr: &str) -> (SimWorld, SimTcpStream, SimTcpStream) {
    let mut sim = world();
    let server_provider = sim.network_provider(server_ip());
    let listener = drive(&mut sim, server_provider.bind(addr)).expect("bind");
    let client_provider = sim.network_provider(client_ip());
    let client = drive(&mut sim, client_provider.connect(addr)).expect("connect");
    let (server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();
    (sim, client, server)
}

fn connected() -> (SimWorld, SimTcpStream, SimTcpStream) {
    connected_at("10.0.1.2:8080")
}

/// Write `data` and drain it onto the wire.
fn put_on_the_wire(sim: &mut SimWorld, stream: &mut SimTcpStream, data: &[u8]) {
    assert_eq!(
        poll_write_once(stream, data).map(|result| result.expect("write is accepted")),
        Poll::Ready(data.len())
    );
    assert!(sim.step(), "the engine drains the queued send");
    assert_eq!(sim.in_flight_bytes(stream.connection_id()), data.len());
}

/// Read until the stream gives an error, and return when it came.
fn time_of_reset(sim: &mut SimWorld, stream: &mut SimTcpStream) -> Duration {
    let mut byte = [0_u8; 1];
    let result = drive(sim, stream.read(&mut byte));
    assert!(
        result.is_err(),
        "the peer's death is an error, got {result:?}"
    );
    sim.now()
}

/// The last reply a process wrote before it died still lands, and its peer
/// sees the reset one link latency after the kill, never at the same
/// instant.
#[test]
fn a_killed_process_resets_its_peer_after_its_last_bytes() {
    const REPLY: &[u8] = b"last-reply";
    let (mut sim, mut client, mut server) = connected();
    put_on_the_wire(&mut sim, &mut server, REPLY);
    let killed_at = sim.now();
    sim.kill_connections_for_ip(server_ip(), DeathNotice::Reset);

    let mut byte = [0_u8; 1];
    assert!(
        poll_read_once(&mut client, &mut byte).is_pending(),
        "the peer cannot know of the death yet"
    );
    let mut reply = vec![0_u8; REPLY.len()];
    drive(&mut sim, client.read_exact(&mut reply)).expect("the last reply lands");
    assert_eq!(reply, REPLY);
    assert!(time_of_reset(&mut sim, &mut client) > killed_at);
}

/// The RST is on the wire like data: a partition holds it until the heal.
#[test]
fn a_partition_holds_the_reset_of_a_dead_process() {
    let (mut sim, mut client, _server) = connected();
    let killed_at = sim.now();
    sim.partition_pair(server_ip(), client_ip(), CUT);
    sim.kill_connections_for_ip(server_ip(), DeathNotice::Reset);
    assert!(
        time_of_reset(&mut sim, &mut client) >= killed_at + CUT,
        "the reset crosses only after the heal"
    );
}

/// A host that lost power sends nothing: the peer's reader waits, and only
/// the peer's own next segment draws the RST.
#[test]
fn a_power_loss_is_seen_only_when_the_peer_writes() {
    let (mut sim, mut client, _server) = connected();
    sim.kill_connections_for_ip(server_ip(), DeathNotice::Silent);
    sim.run_until_empty();
    let mut byte = [0_u8; 1];
    assert!(
        poll_read_once(&mut client, &mut byte).is_pending(),
        "a silent death leaves the reader waiting"
    );

    let wrote_at = sim.now();
    put_on_the_wire(&mut sim, &mut client, b"ping");
    assert!(
        time_of_reset(&mut sim, &mut client) >= wrote_at + LATENCY * 2,
        "the RST comes back one round trip after the write"
    );
}

/// A random close that cuts a receive side resets it: the read is an
/// error, so a protocol that frames by EOF never takes a cut message as
/// complete.
#[test]
fn a_random_receive_close_is_a_reset_not_an_eof() {
    let (sim, client, mut server) = connected();
    sim.close_connection_asymmetric(client.connection_id(), false, true);
    let mut byte = [0_u8; 1];
    match poll_read_once(&mut server, &mut byte) {
        Poll::Ready(Err(error)) => assert_eq!(error.kind(), io::ErrorKind::ConnectionReset),
        other => panic!("expected a reset, got {other:?}"),
    }
}

/// A listener bound by a name is served by the machine that bound it, so a
/// partition of that machine holds the connection's bytes until the heal.
#[test]
fn a_listener_bound_by_name_has_its_owner_as_server_ip() {
    let (mut sim, mut client, mut server) = connected_at("named-service");
    let cut_at = sim.now();
    sim.partition_pair(client_ip(), server_ip(), CUT);
    assert_eq!(
        poll_write_once(&mut client, b"held").map(|result| result.expect("write is accepted")),
        Poll::Ready(4)
    );
    let mut received = [0_u8; 4];
    drive(&mut sim, server.read_exact(&mut received)).expect("the bytes land after the heal");
    assert_eq!(&received, b"held");
    assert!(
        sim.now() >= cut_at + CUT,
        "the partition of the owner reaches the connection"
    );
}

/// An idle connection can fail too: with the black-hole family on, the idle
/// drop silences both directions of a connection that carries no traffic.
#[test]
fn an_idle_connection_can_be_dropped() {
    buggify_reset();
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.black_hole_probability = 1e-9;
    config.chaos.idle_drop_rate = 100.0;
    let mut sim = SimWorld::new_with_network_config_and_seed(config, 20_261_110);
    let server_provider = sim.network_provider(server_ip());
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(client_ip());
    let client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (server, _) = drive(&mut sim, listener.accept()).expect("accept");

    // Nothing moves on the connection; only time passes.
    let timer = sim.sleep(Duration::from_secs(1));
    drive(&mut sim, timer).expect("the timer fires");
    assert!(sim.is_send_black_holed(client.connection_id()));
    assert!(sim.is_send_black_holed(server.connection_id()));
}

/// With the black-hole family off, the idle drop draws nothing and drops
/// nothing.
#[test]
fn no_idle_drop_without_black_holes() {
    buggify_reset();
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.idle_drop_rate = 100.0;
    let mut sim = SimWorld::new_with_network_config_and_seed(config, 20_261_110);
    let server_provider = sim.network_provider(server_ip());
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(client_ip());
    let client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (_server, _) = drive(&mut sim, listener.accept()).expect("accept");

    let timer = sim.sleep(Duration::from_secs(1));
    drive(&mut sim, timer).expect("the timer fires");
    assert!(!sim.is_send_black_holed(client.connection_id()));
}

/// A write parked on a write clog is a no-progress poll: polling it again
/// and again (a `select!` loop) draws nothing from the simulation stream,
/// as a parked read already does. The chaos rolls run only once the write
/// can make progress.
#[test]
fn a_clogged_write_draws_nothing_however_often_it_is_polled() {
    let (sim, mut client, _server) = connected();
    let mut config = NetworkConfiguration::fast_local();
    config.chaos.random_close_probability = 1e-9;
    config.chaos.random_close_cooldown = Duration::ZERO;
    config.chaos.black_hole_probability = 1e-9;
    config.chaos.black_hole_cooldown = Duration::ZERO;
    config.chaos.idle_drop_rate = 0.0;
    config.chaos.clog_duration = Duration::from_secs(1)..Duration::from_secs(2);
    let mut sim = sim;
    sim.set_network_config(config);
    buggify_init(1.0);

    sim.clog_write(client.connection_id());
    assert!(sim.is_write_clogged(client.connection_id()));
    reset_rng_call_count();
    for _ in 0..10 {
        assert!(poll_write_once(&mut client, b"parked").is_pending());
    }
    let draws = rng_call_count();
    buggify_reset();
    assert_eq!(draws, 0, "a clogged write is not an I/O operation yet");
}

/// Make 4 bytes from the server land, unread, at the client, so that a drop
/// of the client stream is an abort (an RST, not a FIN).
fn unread_bytes_at_the_client(
    sim: &mut SimWorld,
    client: &SimTcpStream,
    server: &mut SimTcpStream,
) {
    put_on_the_wire(sim, server, b"left");
    sim.run_until_empty();
    assert_eq!(sim.unread_bytes(client.connection_id()), 4);
}

/// An abort from a live endpoint reaches its peer through the network: the
/// peer sees the reset one link latency later, never at the same instant.
#[test]
fn an_abort_resets_the_peer_after_the_link_latency() {
    let (mut sim, client, mut server) = connected();
    unread_bytes_at_the_client(&mut sim, &client, &mut server);
    let aborted_at = sim.now();
    drop(client);
    let mut byte = [0_u8; 1];
    assert!(
        poll_read_once(&mut server, &mut byte).is_pending(),
        "the peer cannot know of the abort yet"
    );
    assert_eq!(time_of_reset(&mut sim, &mut server), aborted_at + LATENCY);
}

/// The RST of an abort is on the wire like data: a partition holds it until
/// the heal.
#[test]
fn a_partition_holds_the_reset_of_an_abort() {
    let (mut sim, client, mut server) = connected();
    unread_bytes_at_the_client(&mut sim, &client, &mut server);
    let aborted_at = sim.now();
    sim.partition_pair(client_ip(), server_ip(), Duration::from_hours(1));
    sim.partition_pair(server_ip(), client_ip(), Duration::from_hours(1));
    drop(client);
    assert!(
        time_of_reset(&mut sim, &mut server) >= aborted_at + Duration::from_hours(1),
        "the reset crosses only after the heal"
    );
}

/// `SimWorld::close_connection_abort` follows the same rule, and the bytes
/// the aborting side already put on the wire still land before the RST.
#[test]
fn a_scripted_abort_lets_the_bytes_on_the_wire_land_first() {
    const LAST: &[u8] = b"on-the-wire";
    let (mut sim, mut client, mut server) = connected();
    put_on_the_wire(&mut sim, &mut client, LAST);
    let aborted_at = sim.now();
    sim.close_connection_abort(client.connection_id());
    let mut received = vec![0_u8; LAST.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the bytes on the wire land");
    assert_eq!(received, LAST);
    assert!(time_of_reset(&mut sim, &mut server) > aborted_at);
}

/// A random close that cut only the send side put nothing on the wire, and
/// the bytes it discarded are lost. When the application then drops the
/// stream, the peer must still learn of it: the close sends an RST (a FIN
/// would let the peer take the cut stream as complete), and the RST takes
/// the link latency like data.
#[test]
fn a_drop_after_a_send_side_cut_resets_the_peer() {
    let (mut sim, client, mut server) = connected();
    sim.close_connection_asymmetric(client.connection_id(), true, false);
    let dropped_at = sim.now();
    drop(client);
    let mut byte = [0_u8; 1];
    assert!(
        poll_read_once(&mut server, &mut byte).is_pending(),
        "the peer cannot know of the drop yet"
    );
    assert_eq!(time_of_reset(&mut sim, &mut server), dropped_at + LATENCY);
    assert!(
        matches!(poll_write_once(&mut server, b"late"), Poll::Ready(Err(_))),
        "a write after the reset fails"
    );
}

/// A process that closed a stream with bytes still queued (a partition
/// stalls them) and then dies with its host alive: the kernel owns the
/// closed socket, so the queued bytes and the FIN still go out after the
/// heal. The peer reads them, then EOF, and never waits forever.
#[test]
fn a_kill_after_a_close_still_delivers_the_queued_bytes_and_the_fin() {
    let (mut sim, mut client, mut server) = connected();
    sim.partition_pair(client_ip(), server_ip(), CUT);
    assert_eq!(
        poll_write_once(&mut client, b"abc").map(|result| result.expect("write is accepted")),
        Poll::Ready(3)
    );
    assert!(sim.step(), "the queued send stalls under the cut");
    assert_eq!(sim.queued_send_bytes(client.connection_id()), 3);
    drop(client);
    sim.kill_connections_for_ip(client_ip(), DeathNotice::Reset);

    let mut received = [0_u8; 3];
    drive(&mut sim, server.read_exact(&mut received)).expect("the queued bytes land");
    assert_eq!(&received, b"abc");
    let mut byte = [0_u8; 1];
    assert_eq!(
        drive(&mut sim, server.read(&mut byte)).expect("then the FIN"),
        0
    );
}

/// A host that lost power takes the closed socket with it: the queued bytes
/// and the FIN are gone, and the peer learns of the death only when its own
/// segment draws the RST.
#[test]
fn a_power_loss_after_a_close_drops_the_queued_bytes() {
    let (mut sim, mut client, mut server) = connected();
    sim.partition_pair(client_ip(), server_ip(), CUT);
    assert_eq!(
        poll_write_once(&mut client, b"abc").map(|result| result.expect("write is accepted")),
        Poll::Ready(3)
    );
    assert!(sim.step(), "the queued send stalls under the cut");
    drop(client);
    sim.kill_connections_for_ip(client_ip(), DeathNotice::Silent);
    sim.run_until_empty();
    assert_eq!(sim.unread_bytes(server.connection_id()), 0);
    let mut byte = [0_u8; 1];
    assert!(poll_read_once(&mut server, &mut byte).is_pending());
}
