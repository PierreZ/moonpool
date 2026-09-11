//! `AsyncWrite::close` is a write shutdown, not a connection close.
//!
//! The production provider wraps `tokio::net::TcpStream` in a compat layer
//! that maps `poll_close` onto tokio's `poll_shutdown`, i.e. `shutdown(SHUT_WR)`:
//! a FIN behind the queued bytes, the read half left open. The simulated
//! stream used to close the whole connection instead — marking the endpoint
//! closed and discarding its receive buffer — so an EOF-delimited
//! request/response protocol that works in production lost its reply in
//! simulation.

use futures::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    task::noop_waker,
};
use moonpool_sim::{
    NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait, buggify_reset,
    network::sim::SimTcpStream,
};
use std::{
    future::Future,
    io,
    net::IpAddr,
    pin::{Pin, pin},
    task::{Context, Poll},
};

const MAX_DRIVER_STEPS: usize = 10_000;

fn client_ip() -> IpAddr {
    "10.0.1.1".parse().expect("valid test IP")
}

fn server_ip() -> IpAddr {
    "10.0.1.2".parse().expect("valid test IP")
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

/// An established, settled connection between `10.0.1.1` and `10.0.1.2`.
fn connected() -> (SimWorld, SimTcpStream, SimTcpStream) {
    buggify_reset();
    let mut sim =
        SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), 20_260_911);
    let server_provider = sim.network_provider(server_ip());
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(client_ip());
    let client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();
    assert_eq!(sim.pending_event_count(), 0, "the handshake has settled");
    (sim, client, server)
}

/// The EOF-delimited protocol: the client sends its request and shuts down
/// its write side; the server reads to EOF, answers, and closes; the client
/// reads the whole answer.
#[test]
fn a_write_shutdown_keeps_the_read_half_open_for_the_reply() {
    const REQUEST: &[u8] = b"GET /the-thing";
    const REPLY: &[u8] = b"here is the thing";
    let (mut sim, mut client, mut server) = connected();

    drive(&mut sim, client.write_all(REQUEST)).expect("request accepted");
    drive(&mut sim, client.close()).expect("write shutdown succeeds");

    let mut request = Vec::new();
    drive(&mut sim, server.read_to_end(&mut request)).expect("the server reads to EOF");
    assert_eq!(request, REQUEST, "the request lands whole, then EOF");

    drive(&mut sim, server.write_all(REPLY)).expect("the reply is accepted after the peer's FIN");
    drop(server);

    let mut reply = Vec::new();
    drive(&mut sim, client.read_to_end(&mut reply)).expect("the client reads to EOF");
    assert_eq!(
        reply, REPLY,
        "the reply must survive the client's own write shutdown"
    );
}

/// After the shutdown the send side is shut, exactly as after `SHUT_WR`.
#[test]
fn a_write_after_the_shutdown_is_refused() {
    let (mut sim, mut client, mut server) = connected();
    drive(&mut sim, client.close()).expect("write shutdown succeeds");

    let error = match poll_write_once(&mut client, b"late") {
        Poll::Ready(Err(error)) => error,
        other => panic!("a write after shutdown must fail, got {other:?}"),
    };
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

    // The peer still sees exactly one EOF, and can still write back.
    let mut byte = [0_u8; 1];
    assert_eq!(drive(&mut sim, server.read(&mut byte)).expect("EOF"), 0);
    drive(&mut sim, server.write_all(b"still open this way")).expect("peer writes back");
    let mut reply = vec![0_u8; 19];
    drive(&mut sim, client.read_exact(&mut reply)).expect("and the client still reads");
    assert_eq!(&reply, b"still open this way");
}

/// Dropping a stream whose send side is already shut completes the close:
/// the connection is closed, and no second FIN reaches the peer.
#[test]
fn dropping_after_the_shutdown_closes_the_connection_once() {
    let (mut sim, mut client, mut server) = connected();
    let id = client.connection_id();
    drive(&mut sim, client.close()).expect("write shutdown succeeds");
    assert!(
        !sim.is_connection_closed(id),
        "a write shutdown leaves the connection open"
    );

    drop(client);
    assert!(
        sim.is_connection_closed(id),
        "the drop closes the connection"
    );
    sim.run_until_empty();

    let mut byte = [0_u8; 1];
    assert_eq!(drive(&mut sim, server.read(&mut byte)).expect("EOF"), 0);
    assert!(
        matches!(poll_read_once(&mut server, &mut byte), Poll::Ready(Ok(0))),
        "EOF is sticky, never a second event"
    );
}
