//! TCP through `CompioNetworkProvider` on loopback.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};
use moonpool_compio::{
    CompioNetworkProvider, CompioTaskProvider, CompioTcpListener, CompioTcpStream,
    CompioTimeProvider,
};
use moonpool_core::{NetworkProvider, TaskProvider, TcpListenerTrait, TimeProvider};

const MEBIBYTE: usize = 1024 * 1024;

/// A payload of `len` bytes that is not a repetition of a power of two.
fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| u8::try_from(at % 251).expect("fits"))
        .collect()
}

/// A listener on a loopback port, and a client stream connected to it.
async fn connected_pair() -> (CompioTcpListener, CompioTcpStream) {
    let network = CompioNetworkProvider::new();
    let listener = network.bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    let client = network.connect(&address).await.expect("connect");
    (listener, client)
}

/// One MiB out and the same MiB back, the client writing while it reads:
/// `write_all` accepts the payload in bounded chunks, each sent on its way
/// before the next is accepted.
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
    let payload = payload(MEBIBYTE);
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

/// The regression test for buffered writes: a request written and a reply
/// awaited with no flush on either side. A stream that holds written bytes
/// until a flush hangs here, while the simulator and Tokio deliver them.
#[compio::test]
async fn a_request_and_its_reply_need_no_flush() {
    let (listener, mut client) = connected_pair().await;
    let server = CompioTaskProvider::new().spawn_task("reply", async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = [0_u8; 5];
        stream.read_exact(&mut request).await.expect("server read");
        assert_eq!(&request, b"ping?");
        stream.write_all(b"pong!").await.expect("server write");
        // Keep the stream open until the client has its reply.
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.expect("server drain");
    });

    client.write_all(b"ping?").await.expect("client write");
    let mut reply = [0_u8; 5];
    client.read_exact(&mut reply).await.expect("client read");
    assert_eq!(&reply, b"pong!");
    drop(client);
    server.await.expect("the reply server finished");
}

/// Eight MiB echoed through a server that reads slowly: the client's sends
/// outrun the loopback buffers, so its writes wait on backpressure (and may
/// be short), and the echo still arrives byte for byte.
#[compio::test]
async fn a_slow_reader_pushes_back_without_losing_bytes() {
    const LEN: usize = 8 * MEBIBYTE;
    let (listener, client) = connected_pair().await;
    let server = CompioTaskProvider::new().spawn_task("slow echo", async move {
        let time = CompioTimeProvider::new();
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut chunk = vec![0_u8; 64 * 1024];
        loop {
            time.sleep(Duration::from_millis(1)).await.expect("sleep");
            let read = stream.read(&mut chunk).await.expect("server read");
            if read == 0 {
                break;
            }
            stream
                .write_all(&chunk[..read])
                .await
                .expect("server write");
        }
        stream.close().await.expect("server close");
    });

    let (mut reader, mut writer) = client.split();
    let payload = payload(LEN);
    let send = async {
        writer.write_all(&payload).await.expect("client write");
        writer.close().await.expect("client close");
    };
    // Chunked reads, not `read_to_end`: that one zero-fills the vector's
    // whole spare capacity on every pending poll, quadratic over 8 MiB.
    let receive = async {
        let mut echoed = Vec::with_capacity(LEN);
        let mut chunk = vec![0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut chunk).await.expect("client read");
            if read == 0 {
                break echoed;
            }
            echoed.extend_from_slice(&chunk[..read]);
        }
    };
    let ((), echoed) = futures::join!(send, receive);
    assert_eq!(echoed.len(), LEN);
    assert!(echoed == payload, "the echo differs from what was sent");
    server.await.expect("the slow echo server finished");
}

/// Both directions of one stream busy at once through the `BiLock` split
/// moonpool-rpc uses: each side writes 256 KiB while reading the 256 KiB its
/// peer sends, with no flush.
#[compio::test]
async fn reads_and_writes_run_concurrently_on_a_split_stream() {
    const LEN: usize = 256 * 1024;
    async fn exchange(stream: CompioTcpStream, outgoing: Vec<u8>) -> Vec<u8> {
        let (mut reader, mut writer) = stream.split();
        let send = async {
            writer.write_all(&outgoing).await.expect("write");
        };
        let receive = async {
            let mut incoming = vec![0_u8; LEN];
            reader.read_exact(&mut incoming).await.expect("read");
            incoming
        };
        let ((), incoming) = futures::join!(send, receive);
        incoming
    }

    let (listener, client) = connected_pair().await;
    let from_server: Vec<u8> = payload(LEN).into_iter().rev().collect();
    let from_client = payload(LEN);
    let server_sends = from_server.clone();
    let (done, received) = futures::channel::oneshot::channel();
    let server = CompioTaskProvider::new().spawn_task("exchange", async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let incoming = exchange(stream, server_sends).await;
        done.send(incoming).expect("the test awaits the server");
    });

    let received_by_client = exchange(client, from_client.clone()).await;
    server.await.expect("the exchange server finished");
    let received_by_server = received.await.expect("the server reported");
    assert!(
        received_by_client == from_server,
        "the client got other bytes"
    );
    assert!(
        received_by_server == from_client,
        "the server got other bytes"
    );
}

/// `close` flushes, then shuts the write side down: the peer reads all the
/// bytes and then end of stream, and a later write fails.
#[compio::test]
async fn close_delivers_end_of_stream_and_refuses_writes() {
    let (listener, mut client) = connected_pair().await;
    let (done, received) = futures::channel::oneshot::channel();
    let server = CompioTaskProvider::new().spawn_task("reader", async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.expect("server read");
        done.send(bytes).expect("the test awaits the server");
    });

    client.write_all(b"last words").await.expect("client write");
    client.close().await.expect("client close");
    server.await.expect("the reader finished");
    assert_eq!(received.await.expect("the server reported"), b"last words");

    let error = client
        .write_all(b"too late")
        .await
        .expect_err("a write after close fails");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    client.close().await.expect("closing twice is a no-op");
}

/// A send to a peer that has gone away fails on a later write or flush
/// rather than hanging: the first sends may be accepted by the kernel, and
/// the peer's reset makes a following one fail.
#[compio::test]
async fn writing_to_a_closed_peer_fails() {
    let (listener, mut client) = connected_pair().await;
    let (stream, _) = listener.accept().await.expect("accept");
    drop(stream);

    let mut outcome = Ok(());
    for _ in 0..100 {
        outcome = async {
            client.write_all(&[7_u8; 1024]).await?;
            client.flush().await
        }
        .await;
        if outcome.is_err() {
            break;
        }
        CompioTimeProvider::new()
            .sleep(Duration::from_millis(1))
            .await
            .expect("sleep");
    }
    let error = outcome.expect_err("writing to a closed peer eventually fails");
    assert!(
        matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        ),
        "unexpected error: {error}"
    );
}
