//! Delivery modes, peer recovery and failure monitoring on real TCP (#214).
//!
//! Each test drives the production providers on ephemeral localhost ports.
//! The simulation campaign `sim-rpc-delivery` covers the same contracts
//! under faults; these pin the documented outcomes on real sockets.

#![cfg(feature = "prost")]

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::io::{AsyncRead, AsyncWrite};
use moonpool_core::{Resolver, TokioProviders};
use moonpool_rpc::{
    Acceptor, AccessClass, AddressState, BootstrapAddress, BootstrapClient, Connector,
    EndpointState, ErrorReason, Execution, InboundSharing, IncomingRequest, MethodId, PeerContext,
    PeerPolicy, RequestStream, RetryPolicy, RpcConfig, RpcDriver, RpcError, RpcHandle, RpcMethod,
    SchemaVersion, WellKnownId, WellKnownRef,
};
use tokio::sync::mpsc;

#[derive(Clone, PartialEq, prost::Message)]
struct Ping {
    #[prost(uint64, tag = "1")]
    id: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Pong {
    #[prost(uint64, tag = "1")]
    id: u64,
    /// Which execution of the request produced this reply.
    #[prost(uint32, tag = "2")]
    execution: u32,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Ping;
    type Reply = Pong;
    const METHOD: MethodId = MethodId::new(0xD001);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "delivery.echo";
}

struct Held;
impl RpcMethod for Held {
    type Request = Ping;
    type Reply = Pong;
    const METHOD: MethodId = MethodId::new(0xD002);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "delivery.held";
}

type Driver = tokio::task::JoinHandle<io::Error>;

/// Fast liveness and reconnect timing, so recovery fits a test.
fn fast() -> RpcConfig {
    RpcConfig {
        connect_timeout: Duration::from_secs(1),
        handshake_timeout: Duration::from_secs(1),
        peer: PeerPolicy {
            initial_reconnect_delay: Duration::from_millis(10),
            max_reconnect_delay: Duration::from_millis(50),
            // Generous against scheduler stalls on small test runtimes.
            ping_interval: Duration::from_millis(200),
            ping_timeout: Duration::from_secs(1),
            failure_detection_delay: Duration::from_millis(200),
            ..PeerPolicy::default()
        },
        ..RpcConfig::default()
    }
}

async fn listen_at(address: &str, config: RpcConfig) -> (RpcHandle<TokioProviders>, Driver) {
    let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), address, config)
        .await
        .expect("bind");
    (rpc, tokio::spawn(driver.run()))
}

async fn listen(config: RpcConfig) -> (RpcHandle<TokioProviders>, Driver) {
    listen_at("127.0.0.1:0", config).await
}

fn client_with<U>(config: RpcConfig, upgrade: U) -> (RpcHandle<TokioProviders>, Driver)
where
    U: moonpool_rpc::SessionUpgrade<TokioProviders>,
{
    let (driver, rpc) =
        RpcDriver::client_only_with(TokioProviders::new(), config, upgrade).expect("valid config");
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
                execution: 1,
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

fn outcome<T>(result: &Result<T, RpcError>) -> Option<(ErrorReason, Execution)> {
    result
        .as_ref()
        .err()
        .map(|error| (error.reason().clone(), error.execution()))
}

const LIVE: u8 = 0;
const SEVERED: u8 = 1;
const FROZEN: u8 = 2;

/// A client-side upgrade the test can cut: every session opened so far
/// either fails its I/O (severed) or silently black-holes it (frozen).
/// Sessions opened afterwards are healthy.
#[derive(Clone, Default)]
struct Cuttable {
    switches: Arc<Mutex<Vec<Arc<AtomicU8>>>>,
}

impl Cuttable {
    fn cut(&self, how: u8) {
        let switches =
            std::mem::take(&mut *self.switches.lock().expect("Mutex poisoned: prior panic"));
        for switch in switches {
            switch.store(how, Ordering::SeqCst);
        }
    }

    fn wrap<S>(&self, stream: S, peer: &str) -> (CuttableStream<S>, PeerContext) {
        let switch = Arc::new(AtomicU8::new(LIVE));
        self.switches
            .lock()
            .expect("Mutex poisoned: prior panic")
            .push(Arc::clone(&switch));
        (
            CuttableStream {
                inner: stream,
                switch,
            },
            PeerContext::new(peer),
        )
    }
}

struct CuttableStream<S> {
    inner: S,
    switch: Arc<AtomicU8>,
}

fn reset() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionReset, "cut by the test")
}

impl<S: AsyncRead + Unpin> AsyncRead for CuttableStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self.switch.load(Ordering::SeqCst) {
            SEVERED => Poll::Ready(Err(reset())),
            // Nothing will ever arrive.
            FROZEN => Poll::Pending,
            _ => Pin::new(&mut self.inner).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CuttableStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.switch.load(Ordering::SeqCst) {
            SEVERED => Poll::Ready(Err(reset())),
            FROZEN => Poll::Ready(Ok(buf.len())),
            _ => Pin::new(&mut self.inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.switch.load(Ordering::SeqCst) {
            SEVERED => Poll::Ready(Err(reset())),
            FROZEN => Poll::Ready(Ok(())),
            _ => Pin::new(&mut self.inner).poll_flush(cx),
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

impl<S> Connector<S> for Cuttable
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = CuttableStream<S>;
    async fn connect(&self, stream: S, peer: &str) -> io::Result<(Self::Stream, PeerContext)> {
        Ok(self.wrap(stream, peer))
    }
}

impl<S> Acceptor<S> for Cuttable
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = CuttableStream<S>;
    async fn accept(&self, stream: S, peer: &str) -> io::Result<(Self::Stream, PeerContext)> {
        Ok(self.wrap(stream, peer))
    }
}

/// A server whose handler counts executions, reports each the moment it
/// ran, and holds the first `hold` of them until released (one release
/// each).
struct HeldServer {
    rpc: RpcHandle<TokioProviders>,
    driver: Driver,
    executions: Arc<AtomicU32>,
    executed: mpsc::UnboundedReceiver<u32>,
    release: mpsc::UnboundedSender<()>,
    service: moonpool_rpc::ServiceRef<Held>,
}

async fn held_server(hold: u32) -> HeldServer {
    let (rpc, driver) = listen(fast()).await;
    let (service, mut stream) = rpc.register::<Held>(AccessClass::Public).expect("register");
    let executions = Arc::new(AtomicU32::new(0));
    let (executed_tx, executed) = mpsc::unbounded_channel();
    let (release, mut release_rx) = mpsc::unbounded_channel::<()>();
    let counter = Arc::clone(&executions);
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let execution = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = executed_tx.send(execution);
            if execution <= hold {
                let _ = release_rx.recv().await;
            }
            let _ = reply.send(&Pong {
                id: request.id,
                execution,
            });
        }
    });
    HeldServer {
        rpc,
        driver,
        executions,
        executed,
        release,
        service,
    }
}

/// The handler ran, then the connection died before the reply got back.
/// Reliable delivery retransmits on the next connection and may execute
/// the request twice; the caller sees the second execution's reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reliable_delivery_survives_a_lost_reply_and_may_duplicate() {
    reliable_lost_reply().await;
}

#[tokio::test(flavor = "current_thread")]
async fn reliable_delivery_survives_a_lost_reply_current_thread() {
    reliable_lost_reply().await;
}

async fn reliable_lost_reply() {
    let mut server = held_server(1).await;
    let wire = Cuttable::default();
    let (client, client_driver) = client_with(fast(), wire.clone());
    let held = server.service.bind(&client);
    let call = tokio::spawn(async move { held.get_reply(&Ping { id: 7 }).await });
    assert_eq!(server.executed.recv().await, Some(1));
    // Lost reply: the client's connection dies after execution.
    wire.cut(SEVERED);
    let _ = server.release.send(());
    let reply = call.await.expect("join").expect("reliable reply");
    assert_eq!(reply.id, 7);
    assert_eq!(reply.execution, 2, "the retransmitted copy answered");
    assert_eq!(server.executions.load(Ordering::SeqCst), 2);
    let stats = client.stats().expect("running");
    assert!(stats.retransmissions >= 1, "{stats:?}");
    assert_eq!(stats.retained_calls, 0, "retention released by the reply");
    assert_eq!(stats.pending_calls, 0);
    server.driver.abort();
    client_driver.abort();
    drop(server.rpc);
}

/// Same fault, single attempt: the caller learns the outcome is ambiguous
/// and the request is never sent again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_attempt_reports_ambiguity_and_never_retransmits() {
    let mut server = held_server(1).await;
    let wire = Cuttable::default();
    let (client, client_driver) = client_with(fast(), wire.clone());
    let held = server.service.bind(&client);
    let call = tokio::spawn(async move { held.try_get_reply(&Ping { id: 8 }).await });
    assert_eq!(server.executed.recv().await, Some(1));
    wire.cut(SEVERED);
    let result = call.await.expect("join");
    assert_eq!(
        outcome(&result),
        Some((ErrorReason::Disconnected, Execution::MaybeExecuted))
    );
    let _ = server.release.send(());
    // Plenty of time for a (forbidden) retransmission over a new connection.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.executions.load(Ordering::SeqCst), 1);
    let stats = client.stats().expect("running");
    assert_eq!(stats.retransmissions, 0);
    server.driver.abort();
    client_driver.abort();
}

/// A black-holed connection is detected by the ping timeout, and a reliable
/// call queued on it is delivered over the next connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_timeout_detects_a_black_hole_and_reliable_calls_recover() {
    let (server, server_driver) = listen(fast()).await;
    let (service, stream) = server
        .register::<Echo>(AccessClass::Public)
        .expect("register");
    let _handler = serve_echo(stream);
    let wire = Cuttable::default();
    let (client, client_driver) = client_with(fast(), wire.clone());
    let echo = service.bind(&client);
    echo.try_get_reply(&Ping { id: 1 }).await.expect("healthy");
    wire.cut(FROZEN);
    let reply = echo.get_reply(&Ping { id: 2 }).await.expect("recovered");
    assert_eq!(reply.id, 2);
    let stats = client.stats().expect("running");
    assert!(stats.ping_timeouts >= 1, "{stats:?}");
    assert!(stats.retransmissions >= 1, "{stats:?}");
    server_driver.abort();
    client_driver.abort();
}

/// Disconnects are events at once; the address is failed only after the
/// detection delay (while someone watches, the runtime keeps probing); a
/// same-address restart makes the address available again but leaves the
/// old incarnation's dynamic endpoints dead, while a well-known endpoint
/// answers at the same token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_monitor_separates_disconnects_addresses_and_endpoints() {
    let (server, server_driver) = listen(fast()).await;
    let (dynamic, stream) = server
        .register::<Echo>(AccessClass::Public)
        .expect("register");
    let _handler = serve_echo(stream);
    let (_well_known, wk_stream) = server
        .register_well_known::<Echo>(WellKnownId::new(1), AccessClass::Public)
        .expect("register well-known");
    let _wk_handler = serve_echo(wk_stream);
    let address = dynamic.endpoint().address();
    let (client, client_driver) = client_with(fast(), moonpool_rpc::Plaintext);
    let echo = dynamic.bind(&client);
    echo.get_reply(&Ping { id: 1 }).await.expect("first");
    let monitor = client.failure_monitor().expect("running");
    assert_eq!(monitor.address_state(address), AddressState::Available);

    let disconnected = monitor.on_disconnect(address);
    let failed = monitor.on_failed_for(dynamic.endpoint(), Duration::ZERO, 0.0);
    server_driver.abort();
    let _ = server_driver.await;
    tokio::time::timeout(Duration::from_secs(5), disconnected)
        .await
        .expect("disconnect observed")
        .expect("runtime alive");
    tokio::time::timeout(Duration::from_secs(5), failed)
        .await
        .expect("sustained failure observed")
        .expect("runtime alive");
    assert_eq!(monitor.address_state(address), AddressState::Failed);
    assert_eq!(
        monitor.endpoint_state(&dynamic.endpoint()),
        EndpointState::AddressFailed
    );

    // Same address, new incarnation.
    let Ok((restarted, restarted_driver)) =
        RpcDriver::listen(TokioProviders::new(), &address.to_string(), fast())
            .await
            .map(|(driver, rpc)| (rpc, tokio::spawn(driver.run())))
    else {
        eprintln!("could not rebind {address}; skipping the restart half");
        client_driver.abort();
        return;
    };
    let (_again, wk_again) = restarted
        .register_well_known::<Echo>(WellKnownId::new(1), AccessClass::Public)
        .expect("register well-known again");
    let _wk_again = serve_echo(wk_again);

    // The reliable call to the dead dynamic endpoint ends promptly and
    // terminally; it never becomes a call to the new incarnation.
    let stale = echo.get_reply(&Ping { id: 2 }).await;
    assert_eq!(
        outcome(&stale),
        Some((ErrorReason::StaleIncarnation, Execution::NotAdmitted))
    );
    assert_eq!(monitor.address_state(address), AddressState::Available);
    assert_eq!(
        monitor.endpoint_state(&dynamic.endpoint()),
        EndpointState::StaleIncarnation
    );
    // Remembered: the next call fails without a round trip.
    let before = client.stats().expect("running").calls_failed_fast;
    let again = echo.try_get_reply(&Ping { id: 3 }).await;
    assert_eq!(
        outcome(&again),
        Some((ErrorReason::StaleIncarnation, Execution::NotAdmitted))
    );
    assert_eq!(
        client.stats().expect("running").calls_failed_fast,
        before + 1
    );
    // The well-known endpoint recovered at the same token.
    let well_known = WellKnownRef::<Echo>::new(
        BootstrapAddress::Resolved(address),
        WellKnownId::new(1),
        AccessClass::Public,
    );
    let reply = well_known
        .at(address)
        .bind(&client)
        .get_reply(&Ping { id: 4 })
        .await
        .expect("well-known answers the new incarnation");
    assert_eq!(reply.id, 4);
    restarted_driver.abort();
    client_driver.abort();
}

/// Two listening runtimes calling each other end up sharing exactly one
/// connection, whoever dialed first (or both at once).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_listening_peers_share_one_connection() {
    shared_connection().await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_listening_peers_share_one_connection_current_thread() {
    shared_connection().await;
}

async fn shared_connection() {
    for _ in 0..5 {
        let (left, left_driver) = listen(fast()).await;
        let (right, right_driver) = listen(fast()).await;
        let (left_ref, left_stream) = left
            .register::<Echo>(AccessClass::Public)
            .expect("register");
        let (right_ref, right_stream) = right
            .register::<Echo>(AccessClass::Public)
            .expect("register");
        let _l = serve_echo(left_stream);
        let _r = serve_echo(right_stream);
        let to_right = right_ref.bind(&left);
        let to_left = left_ref.bind(&right);
        // Dial both ways at once.
        let (a, b) = tokio::join!(
            to_right.get_reply(&Ping { id: 1 }),
            to_left.get_reply(&Ping { id: 2 })
        );
        assert!(a.is_ok() && b.is_ok(), "{a:?} {b:?}");
        assert!(
            eventually(|| {
                let (l, r) = (left.stats(), right.stats());
                l.is_some_and(|s| s.connections == 1) && r.is_some_and(|s| s.connections == 1)
            })
            .await,
            "left {:?} right {:?}",
            left.stats(),
            right.stats()
        );
        // And the shared connection carries calls both ways.
        assert!(to_right.try_get_reply(&Ping { id: 3 }).await.is_ok());
        assert!(to_left.try_get_reply(&Ping { id: 4 }).await.is_ok());
        let stats = (
            left.stats().expect("running"),
            right.stats().expect("running"),
        );
        assert_eq!(stats.0.connections + stats.1.connections, 2);
        // One side adopted the other's dial (and, when both dialed at once,
        // each closed the loser).
        assert!(
            stats.0.adopted_connections + stats.1.adopted_connections >= 1,
            "{stats:?}"
        );
        eprintln!(
            "adopted {} redundant {}",
            stats.0.adopted_connections + stats.1.adopted_connections,
            stats.0.replaced_connections + stats.1.replaced_connections
        );
        left_driver.abort();
        right_driver.abort();
    }
}

/// One-way requests get no reply and break no promise; an explicit no-reply
/// leaves the caller waiting under its own deadline; a dropped handle is a
/// broken promise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_way_explicit_no_reply_and_broken_promise_are_distinct() {
    let (server, server_driver) = listen(fast()).await;
    let (service, mut stream) = server
        .register::<Held>(AccessClass::Public)
        .expect("register");
    let (client, client_driver) = client_with(fast(), moonpool_rpc::Plaintext);
    let held = service.bind(&client);

    held.send(&Ping { id: 1 }).expect("queued");
    let one_way = stream.recv().await.expect("one-way request");
    assert!(!one_way.reply.expects_reply());
    assert!(!one_way.reply.send(&Pong::default()), "nobody to answer");

    let waiting = tokio::spawn({
        let held = held.clone();
        async move {
            held.try_get_reply_within(&Ping { id: 2 }, Duration::from_millis(300))
                .await
        }
    });
    let silent = stream.recv().await.expect("request");
    assert!(silent.reply.expects_reply());
    silent.reply.never_reply();
    assert_eq!(
        outcome(&waiting.await.expect("join")),
        Some((ErrorReason::Timeout, Execution::MaybeExecuted))
    );

    let broken = tokio::spawn({
        let held = held.clone();
        async move { held.try_get_reply(&Ping { id: 3 }).await }
    });
    drop(stream.recv().await.expect("request"));
    assert_eq!(
        outcome(&broken.await.expect("join")),
        Some((ErrorReason::BrokenPromise, Execution::MaybeExecuted))
    );
    let stats = server.stats().expect("running");
    assert_eq!(stats.broken_promises, 1, "only the dropped handle broke");
    assert_eq!(stats.explicit_no_replies, 1);
    assert_eq!(stats.one_way_received, 1);
    server_driver.abort();
    client_driver.abort();
}

/// An attempt kept after its caller moved on still yields its late outcome;
/// one dropped instead has its late reply counted and discarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attempts_expose_late_outcomes() {
    let mut server = held_server(2).await;
    let (client, client_driver) = client_with(fast(), moonpool_rpc::Plaintext);
    let held = server.service.bind(&client);

    let attempt = held.attempt(&Ping { id: 1 }).expect("started");
    assert_eq!(server.executed.recv().await, Some(1));
    assert_eq!(attempt.transmitted(), Some(true));
    // The caller would have given up here; the attempt still completes.
    let _ = server.release.send(());
    let late = attempt.await.expect("late outcome");
    assert_eq!(late.execution, 1);

    // A dropped attempt's reply is late and discarded.
    let dropped = held.attempt(&Ping { id: 2 }).expect("started");
    assert_eq!(server.executed.recv().await, Some(2));
    drop(dropped);
    let _ = server.release.send(());
    assert!(
        eventually(|| client.stats().is_some_and(|stats| stats.late_replies == 1)).await,
        "{:?}",
        client.stats()
    );
    server.driver.abort();
    client_driver.abort();
}

/// Cancelling a call while its peer is in reconnect backoff withdraws it:
/// it provably never left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_backoff_is_not_admitted_and_releases_retention() {
    // A port nothing listens on.
    let vacant = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("address")
    };
    let mut config = fast();
    // A wide backoff window, so the cancellation lands inside it.
    config.peer.initial_reconnect_delay = Duration::from_secs(3);
    config.peer.max_reconnect_delay = Duration::from_secs(10);
    let (client, client_driver) = client_with(config, moonpool_rpc::Plaintext);
    let target = moonpool_rpc::ServiceRef::<Echo>::new(
        moonpool_rpc::Endpoint::new(
            vacant,
            moonpool_rpc::Incarnation::from_raw(1),
            moonpool_rpc::EndpointToken::from_parts(0, 0),
        ),
        AccessClass::Public,
    )
    .bind(&client);
    let refused = target.try_get_reply(&Ping { id: 1 }).await;
    assert!(
        matches!(
            outcome(&refused),
            Some((ErrorReason::ConnectFailed(_), Execution::NotAdmitted))
        ),
        "{refused:?}"
    );
    // The next dial waits out the backoff: cancel in that state.
    let attempt = target.attempt(&Ping { id: 2 }).expect("queued");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(attempt.transmitted(), Some(false));
    assert_eq!(attempt.cancel(), Execution::NotAdmitted);
    // A reliable call abandoned the same way releases its retention.
    let reliable = tokio::time::timeout(
        Duration::from_millis(100),
        target.get_reply(&Ping { id: 3 }),
    )
    .await;
    assert!(reliable.is_err(), "still waiting when cancelled");
    let stats = client.stats().expect("running");
    assert_eq!(stats.pending_calls, 0);
    assert_eq!(stats.retained_calls, 0);
    assert!(stats.reconnect_waits >= 1, "{stats:?}");
    let probe = client.probe().expect("running");
    client_driver.abort();
    let _ = client_driver.await;
    assert!(probe.is_released(), "{probe:?}");
}

/// An idle connection is closed without counting as a failure, and the
/// next call simply dials again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_connections_close_and_calls_reconnect() {
    let (server, server_driver) = listen(fast()).await;
    let (service, stream) = server
        .register::<Echo>(AccessClass::Public)
        .expect("register");
    let _handler = serve_echo(stream);
    let mut config = fast();
    config.peer.idle_timeout = Duration::from_millis(150);
    let (client, client_driver) = client_with(config, moonpool_rpc::Plaintext);
    let echo = service.bind(&client);
    echo.try_get_reply(&Ping { id: 1 }).await.expect("first");
    assert!(
        eventually(|| client
            .stats()
            .is_some_and(|stats| stats.idle_closes >= 1 && stats.connections == 0))
        .await,
        "{:?}",
        client.stats()
    );
    let monitor = client.failure_monitor().expect("running");
    assert_eq!(
        monitor.address_state(service.endpoint().address()),
        AddressState::Available,
        "idleness is not failure"
    );
    echo.try_get_reply(&Ping { id: 2 }).await.expect("redialed");
    server_driver.abort();
    client_driver.abort();
}

/// A resolver whose answers the test edits.
#[derive(Clone, Default)]
struct TableResolver {
    table: Arc<Mutex<BTreeMap<String, Vec<SocketAddr>>>>,
}

impl TableResolver {
    fn set(&self, host: &str, address: SocketAddr) {
        self.table
            .lock()
            .expect("Mutex poisoned: prior panic")
            .insert(host.to_string(), vec![address]);
    }
}

impl Resolver for TableResolver {
    async fn resolve(&self, target: &str) -> io::Result<Vec<SocketAddr>> {
        let (host, _) = moonpool_core::split_host_port(target)?;
        self.table
            .lock()
            .expect("Mutex poisoned: prior panic")
            .get(host)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown name"))
    }
}

/// A lookup failure, a connection failure and an endpoint that is not
/// registered yet are distinct; the retry helper re-resolves after a
/// connection failure and reaches the name's new address.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_resolves_invalidates_and_retries() {
    let vacant = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("address")
    };
    let (server, server_driver) = listen(fast()).await;
    let (_wk, stream) = server
        .register_well_known::<Echo>(WellKnownId::new(9), AccessClass::Public)
        .expect("register");
    let _handler = serve_echo(stream);
    let live = server.address().expect("listening");
    let (client, client_driver) = client_with(fast(), moonpool_rpc::Plaintext);
    let resolver = TableResolver::default();
    let bootstrap = BootstrapClient::new(&client, resolver.clone(), Duration::from_mins(1));
    let coordinator = WellKnownRef::<Echo>::new(
        BootstrapAddress::parse("coordinator:4500"),
        WellKnownId::new(9),
        AccessClass::Public,
    );

    let missing = bootstrap
        .try_get_reply(&coordinator, &Ping { id: 1 }, Duration::from_secs(1))
        .await;
    assert!(
        matches!(
            outcome(&missing),
            Some((ErrorReason::LookupFailed(_), Execution::NotAdmitted))
        ),
        "{missing:?}"
    );

    resolver.set("coordinator", vacant);
    let refused = bootstrap
        .try_get_reply(&coordinator, &Ping { id: 2 }, Duration::from_secs(1))
        .await;
    assert!(
        matches!(
            outcome(&refused),
            Some((ErrorReason::ConnectFailed(_), Execution::NotAdmitted))
        ),
        "{refused:?}"
    );
    assert_eq!(
        bootstrap.stats().invalidations,
        1,
        "connect failure drops the cache"
    );

    // Not registered yet at the live address: endpoint failure, retryable.
    let unregistered = WellKnownRef::<Echo>::new(
        BootstrapAddress::Resolved(live),
        WellKnownId::new(10),
        AccessClass::Public,
    );
    let not_found = bootstrap
        .try_get_reply(&unregistered, &Ping { id: 3 }, Duration::from_secs(1))
        .await;
    assert_eq!(
        outcome(&not_found),
        Some((ErrorReason::EndpointNotFound, Execution::NotAdmitted))
    );
    assert!(RetryPolicy::default().permits(&not_found.expect_err("not found")));

    // The name moves to the live server while the helper retries.
    let retrying = tokio::spawn({
        let bootstrap = bootstrap.clone();
        let coordinator = coordinator.clone();
        async move {
            bootstrap
                .retry_get_reply(&coordinator, &Ping { id: 4 }, &RetryPolicy::default())
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    resolver.set("coordinator", live);
    let reply = tokio::time::timeout(Duration::from_secs(10), retrying)
        .await
        .expect("retry finished")
        .expect("join")
        .expect("reached the new address");
    assert_eq!(reply.id, 4);
    let stats = bootstrap.stats();
    assert!(stats.retries >= 1 && stats.lookups >= 2, "{stats:?}");
    server_driver.abort();
    client_driver.abort();
}

/// A listening runtime that shares sessions and one that does not still
/// call each other in both directions: the sharing side never locks the
/// other out, and neither adopts anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_sharing_settings_never_lock_a_peer_out() {
    let mut off = fast();
    off.peer.share_inbound_sessions = InboundSharing::Disabled;
    for (left_config, right_config) in [(fast(), off.clone()), (off.clone(), fast())] {
        let (left, left_driver) = listen(left_config).await;
        let (right, right_driver) = listen(right_config).await;
        let (left_ref, left_stream) = left
            .register::<Echo>(AccessClass::Public)
            .expect("register");
        let (right_ref, right_stream) = right
            .register::<Echo>(AccessClass::Public)
            .expect("register");
        let _l = serve_echo(left_stream);
        let _r = serve_echo(right_stream);
        let to_right = right_ref.bind(&left);
        let to_left = left_ref.bind(&right);
        for round in 0..10 {
            let ping = Ping { id: round };
            let (a, b) = tokio::join!(
                to_right.try_get_reply_within(&ping, Duration::from_secs(5)),
                to_left.try_get_reply_within(&ping, Duration::from_secs(5))
            );
            assert!(a.is_ok() && b.is_ok(), "round {round}: {a:?} {b:?}");
        }
        let (l, r) = (
            left.stats().expect("running"),
            right.stats().expect("running"),
        );
        assert_eq!(
            l.adopted_connections + r.adopted_connections,
            0,
            "{l:?} {r:?}"
        );
        left_driver.abort();
        right_driver.abort();
    }
}

/// A raw session that sends a `Hello`, claims `listen`, and stays open.
async fn claim(address: &str, listen: &str) -> tokio::net::TcpStream {
    use moonpool_rpc::protocol::{
        MIN_PROTOCOL_VERSION, PROTOCOL_MAGIC, PROTOCOL_VERSION, WireMessage, encode_frame,
        encode_message,
    };
    use tokio::io::AsyncWriteExt;
    let hello = encode_frame(
        &encode_message(&WireMessage::Hello {
            magic: PROTOCOL_MAGIC,
            min_version: MIN_PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            incarnation: moonpool_rpc::Incarnation::from_raw(77),
            features: 0,
            max_frame_bytes: 1024,
            listen: listen.parse().ok(),
        }),
        1024,
    )
    .expect("frame");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    stream.write_all(&hello).await.expect("write");
    stream
}

/// A session claiming another host's address is served but never becomes
/// the connection to that address; a same-host claim is adopted (the
/// documented residual); the peer table stays bounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claimed_listen_addresses_are_checked_and_bounded() {
    let mut config = fast();
    config.peer.max_tracked_addresses = 3;
    let (server, server_driver) = listen(config).await;
    let address = server.address().expect("listening").to_string();
    let mut sessions = Vec::new();
    sessions.push(claim(&address, "10.9.9.9:4500").await);
    assert!(
        eventually(|| server
            .stats()
            .is_some_and(|stats| stats.unverified_listen_addresses == 1))
        .await
    );
    let monitor = server.failure_monitor().expect("running");
    assert_eq!(
        monitor.disconnects("10.9.9.9:4500".parse().expect("literal")),
        0,
        "a spoofed claim never touches the claimed address"
    );
    for port in 40_001..40_007 {
        sessions.push(claim(&address, &format!("127.0.0.1:{port}")).await);
    }
    assert!(
        eventually(|| server
            .stats()
            .is_some_and(|stats| { stats.adopted_connections + stats.peer_table_full == 6 }))
        .await,
        "{:?}",
        server.stats()
    );
    let stats = server.stats().expect("running");
    assert_eq!(stats.adopted_connections, 3, "{stats:?}");
    assert!(stats.peers <= 3, "{stats:?}");
    drop(sessions);
    server_driver.abort();
}

/// Dials that always fail at one side and succeed at the other.
#[derive(Clone, Default)]
struct DialSwitch {
    refuse: Arc<std::sync::atomic::AtomicBool>,
}

impl<S> Connector<S> for DialSwitch
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = S;
    async fn connect(&self, stream: S, peer: &str) -> io::Result<(S, PeerContext)> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "one-way"));
        }
        Ok((stream, PeerContext::new(peer)))
    }
}

impl<S> Acceptor<S> for DialSwitch
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = S;
    async fn accept(&self, stream: S, peer: &str) -> io::Result<(S, PeerContext)> {
        Ok((stream, PeerContext::new(peer)))
    }
}

/// One-way reachability: the larger address cannot dial the smaller, which
/// can dial it. After `always_accept_after` of failed dials the larger
/// side adopts the smaller side's session instead of keeping its own
/// hopeless dial, so its reliable call gets through (FDB's
/// `ALWAYS_ACCEPT_DELAY`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_we_cannot_dial_stays_reachable_through_its_dial() {
    let mut config = fast();
    config.peer.always_accept_after = Duration::from_millis(300);
    let switches = [DialSwitch::default(), DialSwitch::default()];
    let mut runtimes = Vec::new();
    for switch in &switches {
        let (driver, rpc) = RpcDriver::listen_with(
            TokioProviders::new(),
            "127.0.0.1:0",
            config.clone(),
            switch.clone(),
        )
        .await
        .expect("bind");
        runtimes.push((rpc, tokio::spawn(driver.run())));
    }
    // The larger address is the one that cannot dial.
    let larger = usize::from(runtimes[1].0.address() > runtimes[0].0.address());
    let smaller = 1 - larger;
    switches[larger].refuse.store(true, Ordering::SeqCst);
    let (big, small) = (&runtimes[larger].0, &runtimes[smaller].0);
    let (small_ref, small_stream) = small
        .register::<Echo>(AccessClass::Public)
        .expect("register");
    let (big_ref, big_stream) = big.register::<Echo>(AccessClass::Public).expect("register");
    let _s = serve_echo(small_stream);
    let _b = serve_echo(big_stream);
    // The larger side starts dialing (and failing) first.
    let stuck = tokio::spawn({
        let to_small = small_ref.bind(big);
        async move { to_small.get_reply(&Ping { id: 1 }).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The smaller side's dial arrives while the larger one is still failing.
    let to_big = big_ref.bind(small);
    assert!(to_big.try_get_reply(&Ping { id: 2 }).await.is_ok());
    let reply = tokio::time::timeout(Duration::from_secs(10), stuck)
        .await
        .expect("delivered through the adopted session")
        .expect("join")
        .expect("reply");
    assert_eq!(reply.id, 1);
    let stats = big.stats().expect("running");
    assert!(stats.accepted_over_stalled_dial >= 1, "{stats:?}");
    for (_, driver) in runtimes {
        driver.abort();
    }
}

/// Retained requests beyond a connection's request cap are all sent again
/// after a reconnect: retransmission is never refused as overloaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retransmission_is_not_bounded_by_the_request_queue() {
    let (server, server_driver) = listen(fast()).await;
    let (service, mut stream) = server
        .register::<Held>(AccessClass::Public)
        .expect("register");
    let wire = Cuttable::default();
    let mut config = fast();
    config.max_queued_requests = 4;
    let (client, client_driver) = client_with(config, wire.clone());
    let held = service.bind(&client);
    let calls = 20u64;
    let mut pending = Vec::new();
    let mut first = Vec::new();
    for id in 0..calls {
        let held = held.clone();
        pending.push(tokio::spawn(
            async move { held.get_reply(&Ping { id }).await },
        ));
        // One at a time, so the request cap is never hit before the cut.
        first.push(stream.recv().await.expect("request"));
    }
    wire.cut(SEVERED);
    drop(first);
    // Every retained request comes back over the next connection.
    let mut seen = std::collections::BTreeSet::new();
    while seen.len() < 20 {
        let IncomingRequest { request, reply } =
            tokio::time::timeout(Duration::from_secs(10), stream.recv())
                .await
                .expect("retransmitted in time")
                .expect("request");
        seen.insert(request.id);
        let _ = reply.send(&Pong {
            id: request.id,
            execution: 2,
        });
    }
    for call in pending {
        let reply = call.await.expect("join").expect("reliable reply");
        assert_eq!(reply.execution, 2);
    }
    assert_eq!(seen.len(), 20);
    let stats = client.stats().expect("running");
    assert!(stats.retransmissions >= calls, "{stats:?}");
    server_driver.abort();
    client_driver.abort();
}
