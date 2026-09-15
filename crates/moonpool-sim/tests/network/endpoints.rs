//! Endpoint ownership: a bind holds an address, a connect needs a listener.
//!
//! The engine used to keep only a set of listener ids: `bind` never checked
//! the address, `connect` published its connection after the delay without
//! looking for a live listener, and dropping a listener released nothing. So
//! a connection to an address nobody had bound succeeded, two listeners
//! could hold one address, and the connection-refused / address-in-use
//! behaviour every startup, shutdown and misconfiguration path depends on
//! never happened in simulation.

use futures::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::noop_waker,
};
use moonpool_sim::{
    NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait, buggify_reset,
};
use std::{
    future::Future,
    io,
    net::IpAddr,
    pin::pin,
    task::{Context, Poll},
};

const MAX_DRIVER_STEPS: usize = 10_000;
const SERVER: &str = "10.0.1.2:8080";

fn client_ip() -> IpAddr {
    "10.0.1.1".parse().expect("valid test IP")
}

fn server_ip() -> IpAddr {
    "10.0.1.2".parse().expect("valid test IP")
}

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

fn world() -> SimWorld {
    buggify_reset();
    SimWorld::new_with_network_config_and_seed(NetworkConfiguration::fast_local(), 20_260_911)
}

fn kind_of<T>(result: io::Result<T>) -> Option<io::ErrorKind> {
    result.err().map(|error| error.kind())
}

/// Nobody bound the address: the connect is refused, as the kernel refuses
/// a SYN to a closed port.
#[test]
fn a_connect_to_an_unbound_address_is_refused() {
    let mut sim = world();
    let client = sim.network_provider(client_ip());
    assert_eq!(
        kind_of(drive(&mut sim, client.connect(SERVER))),
        Some(io::ErrorKind::ConnectionRefused)
    );
}

/// A second bind of a live address fails; once the first listener is
/// dropped the address is free again.
#[test]
fn a_second_bind_of_a_live_address_is_in_use_until_the_first_is_dropped() {
    let mut sim = world();
    let server = sim.network_provider(server_ip());
    let first = drive(&mut sim, server.bind(SERVER)).expect("first bind");
    assert_eq!(
        kind_of(drive(&mut sim, server.bind(SERVER))),
        Some(io::ErrorKind::AddrInUse),
        "two live listeners cannot hold one address"
    );
    drop(first);
    let second = drive(&mut sim, server.bind(SERVER));
    assert!(
        second.is_ok(),
        "the address is free once its listener is gone"
    );
}

/// Port zero resolves to distinct usable listeners, not two aliases of `:0`.
#[test]
fn port_zero_binds_get_distinct_ports_and_serve_independently() {
    let mut sim = world();
    let server = sim.network_provider(server_ip());
    let client = sim.network_provider(client_ip());
    let one = drive(&mut sim, server.bind("10.0.1.2:0")).expect("first ephemeral bind");
    let two = drive(&mut sim, server.bind("10.0.1.2:0")).expect("second ephemeral bind");
    let one_addr = one.local_addr().expect("first local address");
    let two_addr = two.local_addr().expect("second local address");
    let one_socket: std::net::SocketAddr = one_addr.parse().expect("first socket address");
    let two_socket: std::net::SocketAddr = two_addr.parse().expect("second socket address");
    assert_ne!(one_socket.port(), 0);
    assert_ne!(two_socket.port(), 0);
    assert_ne!(one_socket, two_socket);

    let mut first_client = drive(&mut sim, client.connect(&one_addr)).expect("connect first");
    let mut second_client = drive(&mut sim, client.connect(&two_addr)).expect("connect second");
    let (mut first_server, _) = drive(&mut sim, one.accept()).expect("accept first");
    let (mut second_server, _) = drive(&mut sim, two.accept()).expect("accept second");
    drive(&mut sim, first_client.write_all(b"one")).expect("write first");
    drive(&mut sim, second_client.write_all(b"two")).expect("write second");
    let mut first_bytes = [0_u8; 3];
    let mut second_bytes = [0_u8; 3];
    drive(&mut sim, first_server.read_exact(&mut first_bytes)).expect("read first");
    drive(&mut sim, second_server.read_exact(&mut second_bytes)).expect("read second");
    assert_eq!(&first_bytes, b"one");
    assert_eq!(&second_bytes, b"two");
}

#[test]
fn port_zero_skips_a_live_explicit_bind_in_the_dynamic_range() {
    let mut sim = world();
    let server = sim.network_provider(server_ip());
    let explicit = drive(&mut sim, server.bind("10.0.1.2:49152")).expect("explicit bind");
    let ephemeral = drive(&mut sim, server.bind("10.0.1.2:0")).expect("ephemeral bind");
    assert_eq!(
        explicit.local_addr().expect("explicit address"),
        "10.0.1.2:49152"
    );
    assert_eq!(
        ephemeral.local_addr().expect("ephemeral address"),
        "10.0.1.2:49153"
    );
}

/// The whole life of an endpoint: bound, serving, dropped, refusing.
#[test]
fn dropping_the_listener_refuses_later_connects() {
    let mut sim = world();
    let server = sim.network_provider(server_ip());
    let client = sim.network_provider(client_ip());
    let listener = drive(&mut sim, server.bind(SERVER)).expect("bind");

    let mut stream = drive(&mut sim, client.connect(SERVER)).expect("connect while bound");
    let (mut accepted, _) = drive(&mut sim, listener.accept()).expect("accept");
    drive(&mut sim, stream.write_all(b"hi")).expect("write");
    let mut buf = [0_u8; 2];
    drive(&mut sim, accepted.read_exact(&mut buf)).expect("read");
    assert_eq!(&buf, b"hi");

    drop(listener);
    assert_eq!(
        kind_of(drive(&mut sim, client.connect(SERVER))),
        Some(io::ErrorKind::ConnectionRefused),
        "nothing listens once the listener is dropped"
    );
    // The established connection is unaffected by the listener going away.
    drive(&mut sim, stream.write_all(b"!!")).expect("write after unbind");
    drive(&mut sim, accepted.read_exact(&mut buf)).expect("read after unbind");
    assert_eq!(&buf, b"!!");
}

/// A crashed process listens on nothing: its address refuses connects and
/// can be bound again, and the dead listener's eventual drop must not steal
/// the address back from the new one.
#[test]
fn a_crashed_process_releases_its_listeners() {
    let mut sim = world();
    let server = sim.network_provider(server_ip());
    let client = sim.network_provider(client_ip());
    let dead = drive(&mut sim, server.bind(SERVER)).expect("bind before the crash");

    sim.abort_all_connections_for_ip(server_ip());
    assert_eq!(
        kind_of(drive(&mut sim, client.connect(SERVER))),
        Some(io::ErrorKind::ConnectionRefused),
        "a crashed process's address refuses connects"
    );

    let reborn = drive(&mut sim, server.bind(SERVER)).expect("the rebooted process binds again");
    drop(dead);
    let stream = drive(&mut sim, client.connect(SERVER)).expect("connect to the new listener");
    let accepted = drive(&mut sim, reborn.accept());
    assert!(
        accepted.is_ok(),
        "the new listener serves; the old one's drop released nothing"
    );
    drop(stream);
}
