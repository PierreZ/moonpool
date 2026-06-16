//! Network provider contract: the invariants any drop-in for `tokio::net` must hold.
//!
//! Note the `futures::io` extension traits — the Tokio streams are wrapped in
//! `tokio_util::compat::Compat`, so I/O goes through `futures::io`, not
//! `tokio::io`. This mirrors exactly what migrating code must import.

use crate::fixtures::Fixtures;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_core::{NetworkProvider, Providers, TcpListenerTrait};

/// Assert the [`NetworkProvider`] binds, connects, echoes bytes, and reports EOF.
///
/// Written single-task (connect before accept) so it also drives the sim event
/// loop correctly when the sim runner is added later.
pub(crate) async fn network_contract<P: Providers, F: Fixtures>(p: &P, fixtures: &F) {
    let net = p.network();

    let listener = net.bind(&fixtures.bind_addr()).await.expect("bind failed");
    let local = listener.local_addr().expect("local_addr failed");
    assert!(!local.is_empty(), "local_addr() must be non-empty");

    // Connect before accept: the handshake completes via the listen backlog,
    // and (in sim) the pending connection is queued before accept polls.
    let mut client = net
        .connect(&fixtures.connect_addr(&local))
        .await
        .expect("connect failed");
    let (mut server, _peer) = listener.accept().await.expect("accept failed");

    // Byte-for-byte echo round-trip: client -> server -> client.
    client
        .write_all(b"ping")
        .await
        .expect("client write failed");
    let mut received = [0u8; 4];
    server
        .read_exact(&mut received)
        .await
        .expect("server read failed");
    assert_eq!(&received, b"ping", "server must receive exact bytes");

    server
        .write_all(&received)
        .await
        .expect("server write failed");
    let mut echoed = [0u8; 4];
    client
        .read_exact(&mut echoed)
        .await
        .expect("client read failed");
    assert_eq!(&echoed, b"ping", "client must receive its bytes back");

    // Half-close: after the client closes its write half, the server reads EOF.
    client.close().await.expect("client close failed");
    let mut tail = [0u8; 8];
    let n = server
        .read(&mut tail)
        .await
        .expect("server read after FIN failed");
    assert_eq!(n, 0, "server must observe EOF after peer half-closes");

    // Connecting to an unbound address fails (where the runtime rejects it).
    if let Some(unbound) = fixtures.unbound_addr() {
        assert!(
            net.connect(&unbound).await.is_err(),
            "connect to an unbound address must fail"
        );
    }
}
