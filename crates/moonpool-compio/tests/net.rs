//! TCP through `CompioNetworkProvider` on loopback.

use std::net::SocketAddr;

use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};
use moonpool_compio::{CompioNetworkProvider, CompioTaskProvider};
use moonpool_core::{NetworkProvider, TaskProvider, TcpListenerTrait};

const MEBIBYTE: usize = 1024 * 1024;

/// One MiB out and the same MiB back: the client's whole payload sits in
/// the compat write buffer (growing it well past its 8 KiB start) until the
/// flush, and the echo server flushes each chunk it reads.
#[compio::test]
async fn a_mebibyte_echoes_through_both_directions() {
    let network = CompioNetworkProvider::new();
    let listener = network.bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    assert!(address.parse::<SocketAddr>().is_ok_and(|a| a.port() != 0));

    let server = CompioTaskProvider::new().spawn_task("echo", async move {
        let (mut stream, peer) = listener.accept().await.expect("accept");
        let peer: SocketAddr = peer.parse().expect("the peer address parses");
        assert!(peer.ip().is_loopback());
        let mut chunk = vec![0_u8; 64 * 1024];
        loop {
            let read = stream.read(&mut chunk).await.expect("server read");
            if read == 0 {
                break;
            }
            stream
                .write_all(&chunk[..read])
                .await
                .expect("server write");
            stream.flush().await.expect("server flush");
        }
        stream.close().await.expect("server close");
    });

    let stream = network.connect(&address).await.expect("connect");
    let (mut reader, mut writer) = stream.split();
    let payload: Vec<u8> = (0..MEBIBYTE)
        .map(|at| u8::try_from(at % 251).expect("fits"))
        .collect();
    let send = async {
        writer.write_all(&payload).await.expect("client write");
        writer.flush().await.expect("client flush");
        writer.close().await.expect("client close");
    };
    let receive = async {
        let mut echoed = Vec::with_capacity(MEBIBYTE);
        reader.read_to_end(&mut echoed).await.expect("client read");
        echoed
    };
    let ((), echoed) = futures::join!(send, receive);
    assert_eq!(echoed.len(), MEBIBYTE);
    assert!(echoed == payload, "the echo differs from what was sent");
    server.await.expect("the echo server finished");
}

#[compio::test]
async fn connecting_to_a_closed_port_fails() {
    let network = CompioNetworkProvider::new();
    let listener = network.bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    drop(listener);
    assert!(network.connect(&address).await.is_err());
}
