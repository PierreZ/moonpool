//! Real TCP: the production providers on ephemeral localhost ports, on both
//! Tokio runtime flavors.
//!
//! Every contract here is also exercised under simulation by the
//! `moonpool-rpc-sim` campaign; these tests prove the same code runs on real
//! sockets and pin the documented outcomes.

#![cfg(feature = "prost")]

use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_rpc::{
    CodecId, DecodeError, EncodeError, Endpoint, Execution, IncomingRequest, MethodId,
    RequestStream, RpcConfig, RpcDriver, RpcError, RpcHandle, RpcMethod, SchemaId, ServiceRef,
    Wire,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, PartialEq, prost::Message)]
struct Ping {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(string, tag = "2")]
    text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Pong {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(string, tag = "2")]
    text: String,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Ping;
    type Reply = Pong;
    const METHOD: MethodId = MethodId::new(0xEC40);
    const SCHEMA: SchemaId = SchemaId::new(1);
    const NAME: &'static str = "echo";
}

/// Same method, next contract version.
struct EchoV2;
impl RpcMethod for EchoV2 {
    type Request = Ping;
    type Reply = Pong;
    const METHOD: MethodId = MethodId::new(0xEC40);
    const SCHEMA: SchemaId = SchemaId::new(2);
    const NAME: &'static str = "echo.v2";
}

/// A different method with the same shapes.
struct Shout;
impl RpcMethod for Shout {
    type Request = Ping;
    type Reply = Pong;
    const METHOD: MethodId = MethodId::new(0x5407);
    const SCHEMA: SchemaId = SchemaId::new(1);
    const NAME: &'static str = "shout";
}

/// Bytes under an application codec id: the same method and schema as
/// `Echo`, but a body the endpoint must never try to decode.
struct Opaque(Vec<u8>);
impl Wire for Opaque {
    const CODEC: CodecId = CodecId::new(0x8001);
    fn encode(&self, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
        buf.extend_from_slice(&self.0);
        Ok(())
    }
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(bytes.to_vec()))
    }
}

struct EchoOpaque;
impl RpcMethod for EchoOpaque {
    type Request = Opaque;
    type Reply = Pong;
    const METHOD: MethodId = MethodId::new(0xEC40);
    const SCHEMA: SchemaId = SchemaId::new(1);
    const NAME: &'static str = "echo.opaque";
}

/// Holds requests until told to answer, to control ordering.
struct Held;
impl RpcMethod for Held {
    type Request = Ping;
    type Reply = Pong;
    const METHOD: MethodId = MethodId::new(0x401D);
    const SCHEMA: SchemaId = SchemaId::new(1);
    const NAME: &'static str = "held";
}

fn ping(id: u64) -> Ping {
    Ping {
        id,
        text: format!("ping-{id}"),
    }
}

async fn listen(config: RpcConfig) -> (RpcHandle<TokioProviders>, tokio::task::JoinHandle<()>) {
    let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", config)
        .await
        .expect("bind an ephemeral port");
    (rpc, tokio::spawn(driver.run()))
}

fn client_only() -> (RpcHandle<TokioProviders>, tokio::task::JoinHandle<()>) {
    let (driver, rpc) = RpcDriver::client_only(TokioProviders::new(), RpcConfig::default());
    (rpc, tokio::spawn(driver.run()))
}

fn serve_echo<M>(mut stream: RequestStream<M>) -> tokio::task::JoinHandle<()>
where
    M: RpcMethod<Request = Ping, Reply = Pong>,
{
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let _ = reply.send(&Pong {
                id: request.id,
                text: request.text,
            });
        }
    })
}

/// Poll `condition` on real time for up to five seconds.
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

async fn two_runtimes_exchange_a_typed_reply() {
    let (server, server_driver) = listen(RpcConfig::default()).await;
    let (client, client_driver) = client_only();
    let (service, stream) = server.register::<Echo>().expect("register");
    let handler = serve_echo(stream);

    // The reference travels as plain bytes, decoded without any runtime.
    let service = ServiceRef::<Echo>::from_bytes(&service.to_bytes()).expect("decodes");
    let echo = service.bind(&client);
    for id in 0..20 {
        let reply = echo.try_get_reply(&ping(id)).await.expect("reply");
        assert_eq!(reply.id, id);
        assert_eq!(reply.text, format!("ping-{id}"));
    }
    let stats = client.stats().expect("running");
    assert_eq!(stats.calls_started, 20);
    assert_eq!(stats.pending_calls, 0);
    assert_eq!(stats.connections, 1, "one session is reused");

    handler.abort();
    server_driver.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn typed_reply_current_thread() {
    two_runtimes_exchange_a_typed_reply().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_reply_multi_thread() {
    two_runtimes_exchange_a_typed_reply().await;
}

/// Two independent OS threads, each with its own current-thread runtime,
/// sharing nothing but the reference bytes.
#[test]
fn two_independent_runtimes_exchange_a_typed_reply() {
    let (address_tx, address_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let (rpc, driver) = listen(RpcConfig::default()).await;
            let (service, stream) = rpc.register::<Echo>().expect("register");
            let handler = serve_echo(stream);
            address_tx.send(service.to_bytes()).expect("send ref");
            tokio::task::spawn_blocking(move || done_rx.recv())
                .await
                .expect("join")
                .expect("done");
            handler.abort();
            driver.abort();
        });
    });
    let bytes = address_rx.recv().expect("reference");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let (rpc, driver) = client_only();
        let service = ServiceRef::<Echo>::from_bytes(&bytes).expect("decodes");
        let reply = service
            .bind(&rpc)
            .try_get_reply(&ping(7))
            .await
            .expect("reply");
        assert_eq!(reply.id, 7);
        driver.abort();
    });
    done_tx.send(()).expect("stop server");
    server.join().expect("server thread");
}

/// The checks every admission runs, observed from a remote and a local
/// caller: both routes must return identical outcomes.
async fn admission_outcomes(
    caller: &RpcHandle<TokioProviders>,
    server: &RpcHandle<TokioProviders>,
) -> Vec<Result<u64, RpcError>> {
    let (service, stream) = server.register::<Echo>().expect("register");
    let handler = serve_echo(stream);
    let endpoint = service.endpoint().clone();
    let mut outcomes = Vec::new();

    let ok = service.bind(caller).try_get_reply(&ping(1)).await;
    outcomes.push(ok.map(|reply| reply.id));
    let wrong_schema = ServiceRef::<EchoV2>::new(endpoint.clone())
        .bind(caller)
        .try_get_reply(&ping(2))
        .await;
    outcomes.push(wrong_schema.map(|reply| reply.id));
    let wrong_method = ServiceRef::<Shout>::new(endpoint.clone())
        .bind(caller)
        .try_get_reply(&ping(3))
        .await;
    outcomes.push(wrong_method.map(|reply| reply.id));
    let wrong_codec = ServiceRef::<EchoOpaque>::new(endpoint.clone())
        .bind(caller)
        .try_get_reply(&Opaque(vec![0xFF; 4]))
        .await;
    outcomes.push(wrong_codec.map(|reply| reply.id));
    let stale = ServiceRef::<Echo>::new(Endpoint::new(
        endpoint.address(),
        moonpool_rpc::Incarnation::from_raw(endpoint.incarnation().get() ^ 1),
        endpoint.token(),
    ))
    .bind(caller)
    .try_get_reply(&ping(4))
    .await;
    outcomes.push(stale.map(|reply| reply.id));

    // Destroy the endpoint: the receiver is owned by the handler task.
    handler.abort();
    let _ = handler.await;
    let destroyed = service.bind(caller).try_get_reply(&ping(5)).await;
    outcomes.push(destroyed.map(|reply| reply.id));

    // Reusing the slot must not redirect the old token.
    let (fresh, fresh_stream) = server.register::<Echo>().expect("register again");
    assert_eq!(fresh.endpoint().token().index(), endpoint.token().index());
    assert_ne!(fresh.endpoint().token(), endpoint.token());
    let fresh_handler = serve_echo(fresh_stream);
    let reused = service.bind(caller).try_get_reply(&ping(6)).await;
    outcomes.push(reused.map(|reply| reply.id));
    let via_fresh = fresh.bind(caller).try_get_reply(&ping(7)).await;
    outcomes.push(via_fresh.map(|reply| reply.id));
    fresh_handler.abort();
    let _ = fresh_handler.await;
    outcomes
}

#[tokio::test(flavor = "current_thread")]
async fn local_and_remote_routes_share_every_admission_contract() {
    let (server, server_driver) = listen(RpcConfig::default()).await;
    let (client, client_driver) = client_only();

    let remote = admission_outcomes(&client, &server).await;
    let local = admission_outcomes(&server, &server).await;
    assert_eq!(remote, local, "local delivery must be indistinguishable");

    let schema = |called, registered| RpcError::SchemaMismatch {
        called: SchemaId::new(called),
        registered: SchemaId::new(registered),
    };
    assert_eq!(
        remote,
        vec![
            Ok(1),
            Err(schema(2, 1)),
            Err(RpcError::MethodMismatch {
                called: Shout::METHOD,
                registered: Echo::METHOD,
            }),
            Err(RpcError::CodecMismatch {
                sent: CodecId::new(0x8001),
                expected: CodecId::PROST,
            }),
            Err(RpcError::StaleIncarnation),
            Err(RpcError::EndpointNotFound),
            Err(RpcError::EndpointNotFound),
            Ok(7),
        ]
    );
    for outcome in remote.iter().filter_map(|outcome| outcome.as_ref().err()) {
        assert_eq!(outcome.execution(), Execution::NotExecuted, "{outcome}");
        assert!(outcome.is_terminal_for_reference(), "{outcome}");
    }
    let stats = server.stats().expect("running");
    assert_eq!(
        stats.requests_admitted, 4,
        "only the good calls reached a handler"
    );

    server_driver.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_reply_handle_is_a_broken_promise_and_overload_is_refused() {
    let config = RpcConfig {
        endpoint_queue_capacity: 1,
        ..RpcConfig::default()
    };
    let (server, server_driver) = listen(config).await;
    let (client, client_driver) = client_only();
    let (service, mut stream) = server.register::<Held>().expect("register");
    let held = service.bind(&client);

    // First call fills the one-slot queue; the second is refused before
    // admission and says so.
    let first = tokio::spawn({
        let held = held.clone();
        async move { held.try_get_reply(&ping(1)).await }
    });
    let Some(incoming) = tokio::time::timeout(Duration::from_secs(5), async {
        // Not taken yet: wait until it is queued, then overflow.
        assert!(eventually(|| server.stats().is_some_and(|s| s.requests_admitted == 1)).await);
        let overflow = held.try_get_reply(&ping(2)).await;
        assert_eq!(overflow, Err(RpcError::Overloaded));
        stream.recv().await
    })
    .await
    .expect("in time") else {
        panic!("request stream ended");
    };
    drop(incoming.reply);
    assert_eq!(first.await.expect("join"), Err(RpcError::BrokenPromise));
    assert_eq!(
        RpcError::BrokenPromise.execution(),
        Execution::MaybeExecuted
    );

    // Oversized requests never leave the process.
    let small = RpcConfig {
        max_frame_bytes: 64,
        ..RpcConfig::default()
    };
    let (tiny, tiny_driver) = listen(small).await;
    let big = Ping {
        id: 0,
        text: "x".repeat(100),
    };
    let outcome = service.bind(&tiny).try_get_reply(&big).await;
    assert!(matches!(outcome, Err(RpcError::FrameTooLarge { .. })));

    tiny_driver.abort();
    server_driver.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_caller_releases_its_route_and_the_late_reply_is_discarded() {
    let (server, server_driver) = listen(RpcConfig::default()).await;
    let (client, client_driver) = client_only();
    let (service, mut stream) = server.register::<Held>().expect("register");
    let held = service.bind(&client);

    let cancelled =
        tokio::time::timeout(Duration::from_millis(50), held.try_get_reply(&ping(1))).await;
    assert!(cancelled.is_err(), "the caller gave up");
    assert_eq!(client.stats().expect("running").pending_calls, 0);
    assert_eq!(client.stats().expect("running").calls_abandoned, 1);

    // The server still executes it: cancellation is not retraction.
    let incoming = stream.recv().await.expect("request arrives anyway");
    assert_eq!(incoming.request.id, 1);
    assert!(incoming.reply.send(&Pong {
        id: 1,
        text: String::new(),
    }));
    assert!(
        eventually(|| client.stats().is_some_and(|stats| stats.late_replies == 1)).await,
        "late reply counted and discarded"
    );

    server_driver.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_after_receipt_is_ambiguous_and_dropped_listener_refuses() {
    let (server, server_driver) = listen(RpcConfig::default()).await;
    let (client, client_driver) = client_only();
    let (service, mut stream) = server.register::<Held>().expect("register");
    let probe = server.probe().expect("running");
    let held = service.bind(&client);

    let call = tokio::spawn({
        let held = held.clone();
        async move { held.try_get_reply(&ping(1)).await }
    });
    // The server has the request (it executed), then its runtime dies.
    let incoming = stream.recv().await.expect("received");
    server_driver.abort();
    let _ = server_driver.await;
    let outcome = call.await.expect("join");
    assert_eq!(outcome, Err(RpcError::Disconnected { transmitted: true }));
    assert_eq!(outcome.unwrap_err().execution(), Execution::MaybeExecuted);

    // The runtime and everything it owned are gone; replying is a no-op.
    assert!(!incoming.reply.send(&Pong::default()));
    assert!(
        stream.recv().await.is_none(),
        "receiver ends with its runtime"
    );
    assert!(probe.is_released(), "{probe:?}");
    assert!(server.register::<Echo>().is_err());

    // Nothing listens any more: the next attempt is refused, never sent.
    let refused = held.try_get_reply(&ping(2)).await;
    assert!(
        matches!(refused, Err(RpcError::ConnectFailed(_))),
        "{refused:?}"
    );
    assert_eq!(refused.unwrap_err().execution(), Execution::NotExecuted);

    // Same address, new process incarnation: old references are stale.
    let address = service.endpoint().address().to_string();
    let (restarted, restarted_driver) =
        match RpcDriver::listen(TokioProviders::new(), &address, RpcConfig::default()).await {
            Ok((driver, rpc)) => (rpc, tokio::spawn(driver.run())),
            Err(error) => {
                // The port can be taken by another process in the meantime;
                // the stale-incarnation path is also covered locally above.
                eprintln!("could not rebind {address}: {error}");
                client_driver.abort();
                return;
            }
        };
    let (_fresh, fresh_stream) = restarted.register::<Held>().expect("register");
    assert_eq!(
        held.try_get_reply(&ping(3)).await,
        Err(RpcError::StaleIncarnation)
    );
    drop(fresh_stream);
    restarted_driver.abort();
    client_driver.abort();
}

async fn raw_session(address: &str) -> tokio::net::TcpStream {
    tokio::net::TcpStream::connect(address)
        .await
        .expect("connect")
}

/// Read until the server closes the connection; returns whether it did.
async fn closed_by_server(stream: &mut tokio::net::TcpStream) -> bool {
    let mut sink = [0; 256];
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => return true,
                Ok(_) => {}
            }
        }
    })
    .await
    .unwrap_or(false)
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_oversized_and_corrupt_input_close_the_session_observably() {
    let config = RpcConfig {
        max_frame_bytes: 1024,
        ..RpcConfig::default()
    };
    let (server, server_driver) = listen(config).await;
    let (service, stream) = server.register::<Echo>().expect("register");
    let handler = serve_echo(stream);
    let address = service.endpoint().address().to_string();

    let hello = moonpool_rpc::protocol::encode_frame(
        &moonpool_rpc::protocol::encode_message(&moonpool_rpc::protocol::WireMessage::Hello {
            magic: moonpool_rpc::protocol::PROTOCOL_MAGIC,
            version: moonpool_rpc::protocol::PROTOCOL_VERSION,
            incarnation: moonpool_rpc::Incarnation::from_raw(9),
        }),
        1024,
    )
    .expect("frame");
    let mut corrupt = hello.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0x10;
    let mut oversized = Vec::new();
    oversized.extend_from_slice(&(1_000_000u32).to_le_bytes());
    oversized.extend_from_slice(&[0; 8]);
    let garbage_envelope =
        moonpool_rpc::protocol::encode_frame(&[0x7F, 1, 2, 3], 1024).expect("frame");
    let wrong_version = moonpool_rpc::protocol::encode_frame(
        &moonpool_rpc::protocol::encode_message(&moonpool_rpc::protocol::WireMessage::Hello {
            magic: moonpool_rpc::protocol::PROTOCOL_MAGIC,
            version: 999,
            incarnation: moonpool_rpc::Incarnation::from_raw(9),
        }),
        1024,
    )
    .expect("frame");

    let cases: [(&str, Vec<u8>); 4] = [
        ("checksum", corrupt),
        ("oversized", oversized),
        ("envelope", [hello.clone(), garbage_envelope].concat()),
        ("version", wrong_version),
    ];
    for (index, (name, bytes)) in cases.into_iter().enumerate() {
        let mut session = raw_session(&address).await;
        session.write_all(&bytes).await.expect("write");
        assert!(closed_by_server(&mut session).await, "{name}: closed");
        let violations = index as u64 + 1;
        assert!(
            eventually(|| server
                .stats()
                .is_some_and(|stats| stats.protocol_violations == violations))
            .await,
            "{name}: counted"
        );
    }
    let stats = server.stats().expect("running");
    assert_eq!(stats.checksum_failures, 1);
    assert_eq!(stats.requests_admitted, 0, "nothing reached a handler");

    // A clean session still works after all that.
    let (client, client_driver) = client_only();
    let reply = service
        .bind(&client)
        .try_get_reply(&ping(1))
        .await
        .expect("healthy");
    assert_eq!(reply.id, 1);

    handler.abort();
    server_driver.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn references_are_runtime_free_and_typed() {
    let (server, server_driver) = listen(RpcConfig::default()).await;
    let (service, _stream) = server.register::<Echo>().expect("register");
    let bytes = service.to_bytes();
    // Decoding needs no runtime, and never as the wrong method type.
    server_driver.abort();
    let _ = server_driver.await;
    assert_eq!(
        ServiceRef::<Echo>::from_bytes(&bytes).expect("decodes"),
        service
    );
    assert!(ServiceRef::<EchoV2>::from_bytes(&bytes).is_err());
    assert!(ServiceRef::<Shout>::from_bytes(&bytes).is_err());
    assert!(ServiceRef::<EchoOpaque>::from_bytes(&bytes).is_err());
    for cut in 0..bytes.len() {
        assert!(ServiceRef::<Echo>::from_bytes(&bytes[..cut]).is_err());
    }
    // Clients bound to a dead runtime fail cleanly.
    let (orphan, orphan_driver) = client_only();
    orphan_driver.abort();
    let _ = orphan_driver.await;
    assert_eq!(
        service.bind(&orphan).try_get_reply(&ping(1)).await,
        Err(RpcError::Shutdown)
    );
}
