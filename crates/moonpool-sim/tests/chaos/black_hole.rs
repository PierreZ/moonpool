//! A black-holed connection accepts every write and delivers nothing.
//!
//! The bytes are acknowledged into the sender's buffer and vanish; the peer's
//! reads stay `Pending` with no data and no EOF, and a graceful close from the
//! holed side never arrives either. The connection looks alive to both ends,
//! which is exactly what a request without a timeout cannot survive. Only an
//! abort (`RST`) still crosses, as a kernel reset would once the host is back.
//! A black hole is permanent for the connection: only a new connection is
//! clean.

use futures::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    task::noop_waker,
};
use moonpool_sim::{
    NetworkConfiguration, NetworkProvider, SimFaultEvent, SimWorld, TcpListenerTrait, buggify_init,
    buggify_reset,
};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build local runtime")
}

/// One poll of a read with a no-op waker, after the world has been drained.
/// `Pending` means no data and no EOF: nothing is left that could arrive.
fn poll_read_once(
    stream: &mut (impl AsyncRead + Unpin),
    buf: &mut [u8],
) -> Poll<io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_read(&mut context, buf)
}

#[test]
fn a_black_holed_send_side_accepts_writes_and_delivers_nothing() {
    local_runtime().block_on(async move {
        let mut sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));
        let addr = "hole-send";
        let listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();
        let mut client = super::drive(&mut sim, provider.connect(addr))
            .await
            .unwrap();
        let (mut server, _) = super::drive(&mut sim, listener.accept()).await.unwrap();

        sim.black_hole_connection(client.connection_id(), true, false);
        assert!(sim.is_send_black_holed(client.connection_id()));
        assert!(!sim.is_send_black_holed(server.connection_id()));
        assert!(
            sim.take_faults()
                .iter()
                .any(|record| matches!(&record.event, SimFaultEvent::BlackHole { direction, .. } if direction == "send"))
        );

        // The write succeeds: the sender is told nothing.
        client
            .write_all(b"into the void")
            .await
            .expect("a black-holed write is accepted");
        sim.run_until_empty();
        let mut buf = [0_u8; 64];
        assert!(
            poll_read_once(&mut server, &mut buf).is_pending(),
            "the peer must see neither data nor EOF"
        );

        // The other direction is untouched.
        server.write_all(b"pong").await.expect("write");
        sim.run_until_empty();
        let n = client.read(&mut buf).await.expect("read");
        assert_eq!(&buf[..n], b"pong");

        // A graceful close is a send too: the FIN vanishes with the data.
        client.close().await.expect("close");
        sim.run_until_empty();
        assert!(
            poll_read_once(&mut server, &mut buf).is_pending(),
            "the peer must not see EOF from a black-holed close"
        );
    });
}

#[test]
fn a_black_holed_recv_side_swallows_the_peers_sends() {
    local_runtime().block_on(async move {
        let mut sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));
        let addr = "hole-recv";
        let listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();
        let mut client = super::drive(&mut sim, provider.connect(addr))
            .await
            .unwrap();
        let (mut server, _) = super::drive(&mut sim, listener.accept()).await.unwrap();

        // "recv" on the client is "send" on the server endpoint.
        sim.black_hole_connection(client.connection_id(), false, true);
        assert!(!sim.is_send_black_holed(client.connection_id()));
        assert!(sim.is_send_black_holed(server.connection_id()));

        server.write_all(b"reply").await.expect("write");
        sim.run_until_empty();
        let mut buf = [0_u8; 64];
        assert!(
            poll_read_once(&mut client, &mut buf).is_pending(),
            "the reply must never reach the client"
        );

        client.write_all(b"request").await.expect("write");
        sim.run_until_empty();
        let n = server.read(&mut buf).await.expect("read");
        assert_eq!(&buf[..n], b"request", "requests still get through");
    });
}

#[test]
fn an_abort_still_crosses_a_black_hole() {
    local_runtime().block_on(async move {
        let mut sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));
        let addr = "hole-abort";
        let listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();
        let mut client = super::drive(&mut sim, provider.connect(addr))
            .await
            .unwrap();
        let (mut server, _) = super::drive(&mut sim, listener.accept()).await.unwrap();

        sim.black_hole_connection(client.connection_id(), true, true);
        client.write_all(b"lost").await.expect("write");
        sim.run_until_empty();
        let mut buf = [0_u8; 64];
        assert!(poll_read_once(&mut server, &mut buf).is_pending());

        // A process kill aborts its connections; the peer must find out.
        sim.close_connection_abort(client.connection_id());
        assert!(
            poll_read_once(&mut server, &mut buf).is_ready(),
            "an abort is not a send and is never swallowed"
        );
    });
}

#[test]
fn the_coin_black_holes_a_connection_and_records_it_once() {
    local_runtime().block_on(async move {
        buggify_init(1.0);
        let mut config = NetworkConfiguration::fast_local();
        config.chaos.black_hole_probability = 1.0;
        config.chaos.black_hole_cooldown = Duration::ZERO;
        let mut sim = SimWorld::new_with_network_config_and_seed(config, 20_260_904);
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));
        let addr = "hole-coin";
        let listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();
        let mut client = super::drive(&mut sim, provider.connect(addr))
            .await
            .unwrap();
        let (server, _) = super::drive(&mut sim, listener.accept()).await.unwrap();

        // The operation that draws the fault proceeds normally.
        client
            .write_all(b"first")
            .await
            .expect("the write that drew the black hole still succeeds");
        client.write_all(b"second").await.expect("write");
        sim.run_until_empty();

        assert!(
            sim.is_send_black_holed(client.connection_id())
                || sim.is_send_black_holed(server.connection_id()),
            "a certain coin must hole at least one direction"
        );
        let holes = sim
            .take_faults()
            .into_iter()
            .filter(|record| matches!(record.event, SimFaultEvent::BlackHole { .. }))
            .count();
        assert_eq!(holes, 1, "the black hole is recorded once, not per I/O");
        buggify_reset();
    });
}

#[test]
fn the_family_off_holes_nothing() {
    local_runtime().block_on(async move {
        buggify_init(1.0);
        let mut sim = SimWorld::new_with_network_config(NetworkConfiguration::fast_local());
        let provider = sim.network_provider("127.0.0.1".parse().expect("valid ip"));
        let addr = "hole-off";
        let listener = super::drive(&mut sim, provider.bind(addr)).await.unwrap();
        let mut client = super::drive(&mut sim, provider.connect(addr))
            .await
            .unwrap();
        let (mut server, _) = super::drive(&mut sim, listener.accept()).await.unwrap();

        for i in 0..50_u8 {
            client.write_all(&[i]).await.expect("write");
        }
        sim.run_until_empty();
        let mut buf = [0_u8; 64];
        let n = server.read(&mut buf).await.expect("read");
        assert!(n > 0, "with the family off every byte still arrives");
        assert!(
            !sim.take_faults()
                .iter()
                .any(|record| matches!(record.event, SimFaultEvent::BlackHole { .. }))
        );
        buggify_reset();
    });
}
