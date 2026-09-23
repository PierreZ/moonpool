//! Reply streams over real TCP: the production providers on ephemeral
//! localhost ports, on both Tokio runtime flavors.
//!
//! The same contracts run under simulation in the `sim-rpc-streams`
//! campaign; these tests prove the code on real sockets: normal completion,
//! slow readers and writers, multiplexed busy and idle streams, abandonment
//! before and after the first item, mid-stream disconnects, errors under
//! exhausted credit, oversized items, and push-back instead of session
//! closure when a peer stops reading replies.

#![cfg(feature = "prost")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use moonpool_core::TokioProviders;
use moonpool_rpc::protocol::{
    PROTOCOL_MAGIC, PROTOCOL_VERSION, WireMessage, encode_frame, encode_message,
    stream_item_frame_len,
};
use moonpool_rpc::stream::SendError;
use moonpool_rpc::{
    AccessClass, ErrorReason, Execution, IncomingRequest, MethodId, PeerPolicy, RequestStream,
    ResourceLimits, RpcConfig, RpcDriver, RpcHandle, RpcMethod, SchemaVersion, ServiceRef,
    StreamPolicy,
};
use tokio::io::AsyncWriteExt;

#[derive(Clone, PartialEq, prost::Message)]
struct Scan {
    #[prost(uint64, tag = "1")]
    id: u64,
    /// Items to send.
    #[prost(uint64, tag = "2")]
    count: u64,
    /// Payload bytes per item.
    #[prost(uint64, tag = "3")]
    item_bytes: u64,
    /// Pause between items, in milliseconds (a slow writer).
    #[prost(uint64, tag = "4")]
    delay_ms: u64,
    /// How the producer ends: 0 finish, 1 fail with `code`, 2 drop, 3 keep
    /// sending until the stream is closed.
    #[prost(uint32, tag = "5")]
    end: u32,
    #[prost(uint64, tag = "6")]
    code: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Chunk {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(uint64, tag = "2")]
    seq: u64,
    #[prost(bytes = "vec", tag = "3")]
    data: Vec<u8>,
}

struct ScanMethod;
impl RpcMethod for ScanMethod {
    type Request = Scan;
    type Reply = Chunk;
    const METHOD: MethodId = MethodId::new(0x5CA0);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "scan";
    const STREAMING: bool = true;
}

/// A unary method, to check streams and calls share a session fairly.
struct Echo;
impl RpcMethod for Echo {
    type Request = Chunk;
    type Reply = Chunk;
    const METHOD: MethodId = MethodId::new(0xEC40);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

/// What a producer did, as the handler saw it: the producer ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Produced {
    id: u64,
    items: u64,
    /// `None` when it ended the stream itself, else the send error.
    error: Option<SendError>,
    /// The largest in-flight (sent, unconsumed) bytes seen after a send.
    max_in_flight: u64,
    window: u64,
}

type Ledger = Arc<Mutex<Vec<Produced>>>;

fn ledger() -> Ledger {
    Arc::new(Mutex::new(Vec::new()))
}

fn produced(ledger: &Ledger, id: u64) -> Option<Produced> {
    ledger
        .lock()
        .expect("Mutex poisoned: prior task panicked")
        .iter()
        .find(|entry| entry.id == id)
        .cloned()
}

fn chunk(id: u64, seq: u64, bytes: u64) -> Chunk {
    Chunk {
        id,
        seq,
        data: vec![u8::try_from(seq % 251).unwrap_or(0); usize::try_from(bytes).unwrap_or(0)],
    }
}

/// Serve every scan on its own task, recording what each producer did.
fn serve_scans(
    mut requests: RequestStream<ScanMethod>,
    ledger: Ledger,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = requests.recv().await {
            let ledger = Arc::clone(&ledger);
            tokio::spawn(async move {
                let Ok(producer) = reply.into_stream() else {
                    return;
                };
                let mut entry = Produced {
                    id: request.id,
                    items: 0,
                    error: None,
                    max_in_flight: 0,
                    window: producer.window(),
                };
                let mut seq = 0;
                loop {
                    if request.end != 3 && seq >= request.count {
                        break;
                    }
                    if request.delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(request.delay_ms)).await;
                    }
                    match producer
                        .send(&chunk(request.id, seq, request.item_bytes))
                        .await
                    {
                        Ok(()) => {
                            entry.items += 1;
                            entry.max_in_flight = entry.max_in_flight.max(producer.in_flight());
                        }
                        Err(error) => {
                            entry.error = Some(error);
                            break;
                        }
                    }
                    seq += 1;
                }
                if entry.error.is_none() {
                    match request.end {
                        1 => {
                            let _ = producer.fail(request.code);
                        }
                        2 => drop(producer),
                        _ => {
                            let _ = producer.finish();
                        }
                    }
                }
                ledger
                    .lock()
                    .expect("Mutex poisoned: prior task panicked")
                    .push(entry);
            });
        }
    })
}

fn serve_echo(mut requests: RequestStream<Echo>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = requests.recv().await {
            let _ = reply.send(&request);
        }
    })
}

struct Rig {
    server: RpcHandle<TokioProviders>,
    client: RpcHandle<TokioProviders>,
    scan: ServiceRef<ScanMethod>,
    echo: ServiceRef<Echo>,
    ledger: Ledger,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    server_driver: tokio::task::JoinHandle<std::io::Error>,
    client_driver: tokio::task::JoinHandle<std::io::Error>,
}

impl Rig {
    async fn new(server_config: RpcConfig, client_config: RpcConfig) -> Self {
        let (server_driver, server) =
            RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", server_config)
                .await
                .expect("bind an ephemeral port");
        let (client_driver, client) =
            RpcDriver::client_only(TokioProviders::new(), client_config).expect("valid config");
        let (scan, scans) = server
            .register::<ScanMethod>(AccessClass::Public)
            .expect("register");
        let (echo, echoes) = server
            .register::<Echo>(AccessClass::Public)
            .expect("register");
        let ledger = ledger();
        let tasks = vec![serve_scans(scans, Arc::clone(&ledger)), serve_echo(echoes)];
        Self {
            server,
            client,
            scan,
            echo,
            ledger,
            tasks,
            server_driver: tokio::spawn(server_driver.run()),
            client_driver: tokio::spawn(client_driver.run()),
        }
    }

    async fn defaults() -> Self {
        Self::new(RpcConfig::default(), RpcConfig::default()).await
    }

    async fn producer_done(&self, id: u64) -> Produced {
        for _ in 0..500 {
            if let Some(entry) = produced(&self.ledger, id) {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("producer {id} never finished");
    }

    fn stop(self) {
        for task in self.tasks {
            task.abort();
        }
        self.server_driver.abort();
        self.client_driver.abort();
    }
}

fn scan(id: u64, count: u64, item_bytes: u64) -> Scan {
    Scan {
        id,
        count,
        item_bytes,
        delay_ms: 0,
        end: 0,
        code: 0,
    }
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

/// Consume a whole stream, checking order: the items and the terminal.
async fn drain(
    stream: &mut moonpool_rpc::ReplyStream<ScanMethod>,
    id: u64,
) -> (u64, Option<moonpool_rpc::RpcError>) {
    let mut seq = 0;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                assert_eq!((chunk.id, chunk.seq), (id, seq), "items arrive in order");
                seq += 1;
            }
            Err(error) => {
                assert!(stream.next().await.is_none(), "one terminal outcome");
                return (seq, Some(error));
            }
        }
    }
    (seq, None)
}

macro_rules! both_flavors {
    ($scenario:ident, $current:ident, $multi:ident) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $current() {
            $scenario().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $multi() {
            $scenario().await;
        }
    };
}

async fn normal_completion_delivers_every_item_in_order() {
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    let mut stream = client
        .get_reply_stream(&scan(1, 200, 100))
        .expect("stream opens");
    let (items, terminal) = drain(&mut stream, 1).await;
    assert_eq!(items, 200);
    assert!(terminal.is_none(), "a normal end yields None: {terminal:?}");
    let entry = rig.producer_done(1).await;
    assert_eq!(entry.items, 200);
    assert_eq!(entry.error, None);
    let stats = rig.client.stats().expect("running");
    assert_eq!(stats.streams_consuming, 0, "the stream is released");
    assert_eq!(stats.stream_buffered_bytes, 0);
    assert!(
        eventually(|| rig
            .server
            .stats()
            .is_some_and(|stats| stats.streams_producing == 0))
        .await,
        "the producer's slot is released once the end was written"
    );
    rig.stop();
}
both_flavors!(
    normal_completion_delivers_every_item_in_order,
    normal_completion_current_thread,
    normal_completion_multi_thread
);

async fn a_slow_reader_holds_the_producer_to_its_window() {
    let item = 1000;
    let window = stream_item_frame_len(item + 16) * 4;
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    let mut stream = client
        .get_reply_stream_with_window(&scan(2, 40, item as u64), window)
        .expect("stream opens");
    let mut seen = 0;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("an item");
        assert_eq!(chunk.seq, seen);
        seen += 1;
        // The reader never holds more than its window, however slow it is.
        assert!(
            stream.buffered_bytes() <= window,
            "{} > {window}",
            stream.buffered_bytes()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(seen, 40);
    let entry = rig.producer_done(2).await;
    assert!(entry.max_in_flight <= window, "{entry:?} window {window}");
    assert_eq!(entry.window, window);
    let server = rig.server.stats().expect("running");
    assert!(
        server.stream_credit_waits > 0,
        "the producer waited for credit"
    );
    let client_stats = rig.client.stats().expect("running");
    assert!(
        client_stats.stream_acks_popped > 0,
        "items taken from the queue were acknowledged then"
    );
    rig.stop();
}
both_flavors!(
    a_slow_reader_holds_the_producer_to_its_window,
    slow_reader_current_thread,
    slow_reader_multi_thread
);

async fn a_slow_writer_is_acknowledged_on_arrival() {
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    let mut request = scan(3, 10, 10);
    request.delay_ms = 20;
    let mut stream = client.get_reply_stream(&request).expect("stream opens");
    let (items, terminal) = drain(&mut stream, 3).await;
    assert_eq!((items, terminal), (10, None));
    let stats = rig.client.stats().expect("running");
    assert!(
        stats.stream_acks_immediate > 0,
        "a waiting consumer acknowledges an item as it is handed over"
    );
    rig.stop();
}
both_flavors!(
    a_slow_writer_is_acknowledged_on_arrival,
    slow_writer_current_thread,
    slow_writer_multi_thread
);

async fn busy_and_idle_streams_share_one_session() {
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    // A busy stream with large items and a large window keeps the writer
    // saturated, and its consumer stops reading halfway.
    let mut busy = client
        .get_reply_stream_with_window(&scan(10, 2000, 16 * 1024), 4 << 20)
        .expect("busy stream");
    let mut request = scan(11, 3, 8);
    request.delay_ms = 50;
    let mut idle = client.get_reply_stream(&request).expect("idle stream");
    for _ in 0..50 {
        busy.next().await.expect("item").expect("ok");
    }
    // The idle stream's sparse items and unary calls still get through
    // while the busy stream has its whole window queued.
    let started = std::time::Instant::now();
    let (items, terminal) = drain(&mut idle, 11).await;
    assert_eq!((items, terminal), (3, None));
    let echo = rig.echo.bind(&rig.client);
    for id in 0..10 {
        let reply = echo
            .try_get_reply_within(&chunk(id, id, 8), Duration::from_secs(5))
            .await
            .expect("unary call progresses beside a saturated stream");
        assert_eq!(reply.seq, id);
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(rig.client.stats().expect("running").connections, 1);
    drop(busy);
    let entry = rig.producer_done(10).await;
    assert_eq!(entry.error, Some(SendError::Cancelled));
    rig.stop();
}
both_flavors!(
    busy_and_idle_streams_share_one_session,
    multiplexed_current_thread,
    multiplexed_multi_thread
);

async fn abandoning_a_stream_releases_the_producer() {
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    // Before the first item: the handler waits a while before its first
    // send; the caller is gone by then.
    let mut request = scan(20, 5, 10);
    request.delay_ms = 200;
    request.end = 3;
    let stream = client.get_reply_stream(&request).expect("stream opens");
    // Let the request leave, then abandon before anything came back.
    assert!(
        eventually(|| rig
            .server
            .stats()
            .is_some_and(|stats| stats.streams_admitted >= 1))
        .await
    );
    assert_eq!(stream.cancel(), Execution::MaybeExecuted);
    let entry = rig.producer_done(20).await;
    assert_eq!(entry.items, 0);
    assert_eq!(entry.error, Some(SendError::Cancelled));

    // After the first item: the producer keeps sending until told.
    let mut request = scan(21, 0, 10);
    request.end = 3;
    let mut stream = client.get_reply_stream(&request).expect("stream opens");
    let first = stream.next().await.expect("an item").expect("ok");
    assert_eq!(first.seq, 0);
    drop(stream);
    let entry = rig.producer_done(21).await;
    assert!(entry.items >= 1);
    assert_eq!(entry.error, Some(SendError::Cancelled));
    assert!(
        eventually(|| rig
            .server
            .stats()
            .is_some_and(|stats| { stats.streams_producing == 0 && stats.inflight_requests == 0 }))
        .await,
        "no orphan stream after cancellation"
    );
    let stats = rig.client.stats().expect("running");
    assert_eq!(stats.streams_consuming, 0);
    assert_eq!(stats.stream_window_reserved, 0, "every window was released");
    rig.stop();
}
both_flavors!(
    abandoning_a_stream_releases_the_producer,
    abandonment_current_thread,
    abandonment_multi_thread
);

async fn a_mid_stream_disconnect_ends_both_sides() {
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    let mut request = scan(30, 0, 10);
    request.end = 3;
    request.delay_ms = 5;
    let mut stream = client.get_reply_stream(&request).expect("stream opens");
    for seq in 0..3 {
        assert_eq!(stream.next().await.expect("item").expect("ok").seq, seq);
    }
    // The client runtime goes away: its session ends under the producer.
    rig.client_driver.abort();
    let mut error = None;
    while let Some(item) = stream.next().await {
        if let Err(terminal) = item {
            error = Some(terminal);
        }
    }
    let error = error.expect("a terminal error");
    assert_eq!(error.reason(), &ErrorReason::Shutdown);
    assert_eq!(
        error.execution(),
        Execution::Executed,
        "items proved execution"
    );
    let entry = rig.producer_done(30).await;
    assert_eq!(entry.error, Some(SendError::Disconnected));
    for task in rig.tasks {
        task.abort();
    }
    rig.server_driver.abort();
}

async fn a_server_crash_mid_stream_is_a_disconnect() {
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    let mut request = scan(31, 0, 10);
    request.end = 3;
    request.delay_ms = 5;
    let mut stream = client.get_reply_stream(&request).expect("stream opens");
    assert_eq!(stream.next().await.expect("item").expect("ok").seq, 0);
    rig.server_driver.abort();
    let mut error = None;
    while let Some(item) = stream.next().await {
        if let Err(terminal) = item {
            error = Some(terminal);
        }
    }
    let error = error.expect("a terminal error");
    assert_eq!(error.reason(), &ErrorReason::Disconnected);
    assert_eq!(error.execution(), Execution::Executed);
    for task in rig.tasks {
        task.abort();
    }
    rig.client_driver.abort();
}

async fn disconnects() {
    a_mid_stream_disconnect_ends_both_sides().await;
    a_server_crash_mid_stream_is_a_disconnect().await;
}
both_flavors!(
    disconnects,
    mid_stream_disconnect_current_thread,
    mid_stream_disconnect_multi_thread
);

async fn an_error_is_delivered_under_exhausted_credit() {
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    let window = stream_item_frame_len(64) * 2;
    let mut request = scan(40, 2, 32);
    request.end = 1;
    request.code = 7;
    let mut stream = client
        .get_reply_stream_with_window(&request, window)
        .expect("stream opens");
    // Consume nothing until the producer has used the whole window and
    // failed: the end needs no credit.
    let entry = rig.producer_done(40).await;
    assert_eq!(entry.items, 2);
    assert!(
        eventually(|| stream.is_terminated()).await,
        "the error arrived while the window was full"
    );
    let (items, terminal) = drain(&mut stream, 40).await;
    assert_eq!(items, 2, "items sent before the error stay observable");
    let terminal = terminal.expect("the producer's error");
    assert_eq!(terminal.reason(), &ErrorReason::StreamFailed { code: 7 });
    assert_eq!(terminal.execution(), Execution::Executed);
    rig.stop();
}
both_flavors!(
    an_error_is_delivered_under_exhausted_credit,
    exhausted_credit_current_thread,
    exhausted_credit_multi_thread
);

async fn oversized_items_and_mismatches_are_refused_up_front() {
    let rig = Rig::defaults().await;
    let client = rig.scan.bind(&rig.client);
    // An item larger than the window is refused, not parked.
    let window = stream_item_frame_len(100);
    let mut stream = client
        .get_reply_stream_with_window(&scan(50, 1, 5000), window)
        .expect("stream opens");
    let entry = rig.producer_done(50).await;
    assert!(
        matches!(entry.error, Some(SendError::TooLarge { limit, .. }) if limit == window),
        "{entry:?}"
    );
    let terminal = drain(&mut stream, 50).await.1.expect("broken promise");
    assert_eq!(terminal.reason(), &ErrorReason::BrokenPromise);
    // A unary call to a streaming method, and a stream to a unary one.
    let wrong = ServiceRef::<Echo>::from_bytes(&rig.scan.to_bytes());
    assert!(wrong.is_err(), "the reference names another method");
    let stream_to_unary = rig.echo.bind(&rig.client).get_reply_stream(&chunk(1, 1, 1));
    assert!(matches!(
        stream_to_unary.map(|_| ()),
        Err(error) if error.reason() == &ErrorReason::StreamingMismatch { endpoint_streams: false }
            && error.execution() == Execution::NotAdmitted
    ));
    let unary_to_stream = client.try_get_reply(&scan(51, 1, 1)).await;
    assert!(matches!(
        unary_to_stream,
        Err(error) if error.reason() == &ErrorReason::StreamingMismatch { endpoint_streams: true }
            && error.execution() == Execution::NotAdmitted
    ));
    rig.stop();
}
both_flavors!(
    oversized_items_and_mismatches_are_refused_up_front,
    refusals_current_thread,
    refusals_multi_thread
);

async fn a_local_stream_keeps_the_same_contracts() {
    let rig = Rig::defaults().await;
    let local = rig.scan.bind(&rig.server);
    let window = stream_item_frame_len(80) * 3;
    let mut stream = local
        .get_reply_stream_with_window(&scan(60, 30, 50), window)
        .expect("stream opens");
    let mut seen = 0;
    while let Some(item) = stream.next().await {
        assert_eq!(item.expect("ok").seq, seen);
        seen += 1;
        assert!(stream.buffered_bytes() <= window);
    }
    assert_eq!(seen, 30);
    let entry = rig.producer_done(60).await;
    assert!(entry.max_in_flight <= window);
    let mut request = scan(61, 0, 10);
    request.end = 3;
    let mut stream = local.get_reply_stream(&request).expect("stream opens");
    stream.next().await.expect("item").expect("ok");
    drop(stream);
    assert_eq!(
        rig.producer_done(61).await.error,
        Some(SendError::Cancelled)
    );
    assert!(
        eventually(|| rig
            .server
            .stats()
            .is_some_and(|stats| { stats.streams_producing == 0 && stats.streams_consuming == 0 }))
        .await
    );
    rig.stop();
}
both_flavors!(
    a_local_stream_keeps_the_same_contracts,
    local_stream_current_thread,
    local_stream_multi_thread
);

/// A peer that sends requests and never reads replies: the server's writer
/// stalls, replies queue, and new requests are refused `Overloaded` before
/// admission instead of the whole session being closed (the P1 residual).
async fn a_peer_that_stops_reading_is_pushed_back_not_disconnected() {
    let limits = ResourceLimits {
        max_inflight_per_connection: 8,
        ..ResourceLimits::default()
    };
    let config = RpcConfig {
        limits,
        reserved_control_frames: 4096,
        ..RpcConfig::default()
    };
    let (driver, server) = RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", config)
        .await
        .expect("bind");
    let (echo, echoes) = server
        .register::<Echo>(AccessClass::Public)
        .expect("register");
    let handler = serve_echo(echoes);
    let driver = tokio::spawn(driver.run());
    let address = server.address().expect("listening");
    let mut socket = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let hello = WireMessage::Hello {
        magic: PROTOCOL_MAGIC,
        min_version: PROTOCOL_VERSION,
        max_version: PROTOCOL_VERSION,
        incarnation: moonpool_rpc::Incarnation::from_raw(7),
        features: 0,
        max_frame_bytes: 1 << 20,
        listen: None,
    };
    let mut bytes = encode_frame(&encode_message(&hello), u32::MAX).expect("frame");
    let endpoint = echo.endpoint();
    // Replies of 256 KiB each: a few fill the socket buffers.
    let body = prost::Message::encode_to_vec(&chunk(0, 0, 256 * 1024));
    for call_id in 0..200 {
        let request = WireMessage::Request {
            call_id,
            incarnation: endpoint.incarnation(),
            token: endpoint.token(),
            interface: moonpool_rpc::InterfaceId::new(0),
            interface_version: SchemaVersion::new(0),
            method: Echo::METHOD,
            schema: Echo::SCHEMA,
            codec: moonpool_rpc::CodecId::PROST,
            flags: 0,
            stream_window: 0,
            metadata: Vec::new(),
            body: body.clone(),
        };
        bytes.extend(encode_frame(&encode_message(&request), u32::MAX).expect("frame"));
    }
    socket.write_all(&bytes).await.expect("send requests");
    assert!(
        eventually(|| server
            .stats()
            .is_some_and(|stats| stats.overload_refusals > 0))
        .await,
        "requests beyond the in-flight budget were refused"
    );
    assert!(
        eventually(|| server
            .stats()
            .is_some_and(|stats| { stats.requests_admitted + stats.requests_rejected == 200 }))
        .await,
        "every request was admitted or refused: the reader never waited"
    );
    let stats = server.stats().expect("running");
    assert!(stats.inflight_requests <= 8, "{stats:?}");
    assert_eq!(stats.control_reserve_closes, 0);
    assert_eq!(stats.connections, 1, "the session was not closed");
    drop(socket);
    handler.abort();
    driver.abort();
}
both_flavors!(
    a_peer_that_stops_reading_is_pushed_back_not_disconnected,
    push_back_current_thread,
    push_back_multi_thread
);

/// A stream request beyond the stream budget is refused before admission.
#[tokio::test(flavor = "current_thread")]
async fn stream_budgets_refuse_before_admission() {
    let server_config = RpcConfig {
        streams: StreamPolicy {
            max_streams_per_connection: 2,
            ..StreamPolicy::default()
        },
        ..RpcConfig::default()
    };
    let client_config = RpcConfig {
        streams: StreamPolicy {
            max_buffered_bytes: StreamPolicy::default().window_bytes * 3,
            ..StreamPolicy::default()
        },
        ..RpcConfig::default()
    };
    let rig = Rig::new(server_config, client_config).await;
    let client = rig.scan.bind(&rig.client);
    let mut open = Vec::new();
    for id in 0..2 {
        let mut request = scan(70 + id, 0, 1);
        request.end = 3;
        let mut stream = client.get_reply_stream(&request).expect("opens");
        stream.next().await.expect("item").expect("ok");
        open.push(stream);
    }
    // The third is refused by the server's per-connection budget.
    let mut third = client.get_reply_stream(&scan(72, 1, 1)).expect("sent");
    let refused = third.next().await.expect("terminal").expect_err("refused");
    assert_eq!(refused.reason(), &ErrorReason::Overloaded);
    assert_eq!(refused.execution(), Execution::NotAdmitted);
    drop(third);
    // The fourth is refused locally: three windows are all the budget.
    let fourth = client.get_reply_stream(&scan(73, 1, 1));
    let fifth = client.get_reply_stream(&scan(74, 1, 1));
    assert!(fourth.is_ok());
    assert!(matches!(
        fifth.map(|_| ()),
        Err(error) if error.reason() == &ErrorReason::Overloaded
    ));
    drop(open);
    rig.stop();
}

/// Pings, pongs and acknowledgements keep flowing while eight streams keep
/// the session's writer busy: no ping times out, the session is never
/// replaced, and the producers keep being paced by consumption.
async fn control_frames_progress_beside_saturated_streams() {
    let peer = PeerPolicy {
        // A ping every 50 ms plus the 500 ms answer window: each ping must
        // be answered within 500 ms while the writer is saturated.
        ping_interval: Duration::from_millis(50),
        ping_timeout: Duration::from_millis(500),
        ..PeerPolicy::default()
    };
    let config = RpcConfig {
        peer,
        ..RpcConfig::default()
    };
    let rig = Rig::new(config.clone(), config).await;
    let client = rig.scan.bind(&rig.client);
    let mut consumers = Vec::new();
    for id in 0..8 {
        let mut request = scan(100 + id, 0, 16 * 1024);
        request.end = 3;
        let mut stream = client
            .get_reply_stream_with_window(&request, 1 << 20)
            .expect("stream opens");
        consumers.push(tokio::spawn(async move {
            let mut taken = 0u64;
            while let Some(Ok(_)) = stream.next().await {
                taken += 1;
            }
            taken
        }));
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let client_stats = rig.client.stats().expect("running");
    let server_stats = rig.server.stats().expect("running");
    assert!(client_stats.pings_sent >= 4, "{client_stats:?}");
    assert_eq!(client_stats.ping_timeouts, 0, "{client_stats:?}");
    assert_eq!(server_stats.ping_timeouts, 0, "{server_stats:?}");
    assert_eq!(
        client_stats.connections_opened, 1,
        "the session was never replaced"
    );
    assert!(server_stats.stream_acks_received > 0);
    for consumer in &consumers {
        consumer.abort();
    }
    rig.stop();
}
both_flavors!(
    control_frames_progress_beside_saturated_streams,
    control_progress_current_thread,
    control_progress_multi_thread
);

/// A caller whose request queue is full is refused `Overloaded` at once,
/// for calls and streams alike. (The refusal used to drop the call's guard
/// while the runtime's state was locked, and the guard re-locked it: a
/// self-deadlock on the first full queue.)
#[tokio::test(flavor = "current_thread")]
async fn a_full_request_queue_refuses_calls_instead_of_stalling() {
    // A listener that accepts and never answers the handshake: requests
    // stay queued behind it.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = silent.local_addr().expect("address");
    let holder = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = silent.accept().await {
            held.push(socket);
        }
    });
    let config = RpcConfig {
        max_queued_requests: 4,
        ..RpcConfig::default()
    };
    let (driver, client) =
        RpcDriver::client_only(TokioProviders::new(), config).expect("valid config");
    let driver = tokio::spawn(driver.run());
    let endpoint = moonpool_rpc::Endpoint::new(
        address,
        moonpool_rpc::Incarnation::from_raw(1),
        moonpool_rpc::EndpointToken::from_parts(1, 1),
    );
    let echo = ServiceRef::<Echo>::new(endpoint, AccessClass::Public);
    let scans = ServiceRef::<ScanMethod>::new(endpoint, AccessClass::Public);
    let mut attempts = Vec::new();
    let mut refused = 0;
    for id in 0..8 {
        match echo.bind(&client).attempt(&chunk(id, id, 1)) {
            Ok(attempt) => attempts.push(attempt),
            Err(error) => {
                assert_eq!(error.reason(), &ErrorReason::Overloaded);
                assert_eq!(error.execution(), Execution::NotAdmitted);
                refused += 1;
            }
        }
    }
    assert_eq!((attempts.len(), refused), (4, 4));
    let stream = scans.bind(&client).get_reply_stream(&scan(1, 1, 1));
    assert!(matches!(
        stream.map(|_| ()),
        Err(error) if error.reason() == &ErrorReason::Overloaded
    ));
    for attempt in attempts {
        assert_eq!(attempt.cancel(), Execution::NotAdmitted, "withdrawn unsent");
    }
    driver.abort();
    holder.abort();
}

/// Hello, then one stream request announcing `window` for `target`.
fn raw_stream_opening(target: &ServiceRef<ScanMethod>, request: &Scan, window: u64) -> Vec<u8> {
    let hello = WireMessage::Hello {
        magic: PROTOCOL_MAGIC,
        min_version: PROTOCOL_VERSION,
        max_version: PROTOCOL_VERSION,
        incarnation: moonpool_rpc::Incarnation::from_raw(9),
        features: 0,
        max_frame_bytes: 1 << 20,
        listen: None,
    };
    let endpoint = target.endpoint();
    let open = WireMessage::Request {
        call_id: 1,
        incarnation: endpoint.incarnation(),
        token: endpoint.token(),
        interface: moonpool_rpc::InterfaceId::new(0),
        interface_version: SchemaVersion::new(0),
        method: ScanMethod::METHOD,
        schema: ScanMethod::SCHEMA,
        codec: moonpool_rpc::CodecId::PROST,
        flags: moonpool_rpc::protocol::REQUEST_FLAG_STREAM,
        stream_window: window,
        metadata: Vec::new(),
        body: prost::Message::encode_to_vec(request),
    };
    let mut bytes = encode_frame(&encode_message(&hello), u32::MAX).expect("frame");
    bytes.extend(encode_frame(&encode_message(&open), u32::MAX).expect("frame"));
    bytes
}

/// A stream request whose window holds no item is refused as malformed,
/// before any handler, instead of admitting a stream that can never send.
#[tokio::test(flavor = "current_thread")]
async fn a_window_that_holds_no_item_is_refused() {
    use tokio::io::AsyncReadExt;
    let rig = Rig::defaults().await;
    let address = rig.server.address().expect("listening");
    let mut socket = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let request = scan(80, 1, 1);
    socket
        .write_all(&raw_stream_opening(&rig.scan, &request, 10))
        .await
        .expect("send");
    let mut decoder = moonpool_rpc::protocol::FrameDecoder::new(1 << 20);
    let mut chunk = vec![0; 4096];
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let read = socket.read(&mut chunk).await.expect("read");
            assert!(read > 0, "the server closed the session");
            decoder.feed(&chunk[..read]);
            while let Some(payload) = decoder.next_frame().expect("frames") {
                if let Ok(WireMessage::Reply {
                    call_id: 1,
                    outcome,
                }) = moonpool_rpc::protocol::decode_message(&payload)
                {
                    return outcome;
                }
            }
        }
    })
    .await
    .expect("a rejection arrives");
    assert_eq!(
        outcome,
        moonpool_rpc::protocol::WireOutcome::Err(
            moonpool_rpc::protocol::WireError::MalformedRequest
        )
    );
    assert!(produced(&rig.ledger, 80).is_none(), "no handler ran");
    rig.stop();
}

/// Producer budgets: each admitted stream reserves its window from the
/// connection's producer budget, and a stream whose window does not fit is
/// refused before admission. Cancelling that refused stream reports what
/// the refusal proved.
#[tokio::test(flavor = "current_thread")]
async fn producer_windows_are_reserved_from_a_budget() {
    let window = 256 << 10;
    let server_config = RpcConfig {
        streams: StreamPolicy {
            max_producer_bytes_per_connection: 2 * window + window / 2,
            ..StreamPolicy::default()
        },
        ..RpcConfig::default()
    };
    let rig = Rig::new(server_config, RpcConfig::default()).await;
    let client = rig.scan.bind(&rig.client);
    let mut open = Vec::new();
    for id in 0..2 {
        let mut request = scan(90 + id, 0, 1);
        request.end = 3;
        let mut stream = client
            .get_reply_stream_with_window(&request, window)
            .expect("opens");
        stream.next().await.expect("item").expect("ok");
        open.push(stream);
    }
    assert_eq!(
        rig.server
            .stats()
            .expect("running")
            .producer_window_reserved,
        2 * window
    );
    let third = client
        .get_reply_stream_with_window(&scan(92, 1, 1), window)
        .expect("sent");
    assert!(eventually(|| third.is_terminated()).await);
    assert_eq!(
        third.cancel(),
        Execution::NotAdmitted,
        "a refusal before admission proves it never ran"
    );
    assert!(produced(&rig.ledger, 92).is_none());
    drop(open);
    assert!(
        eventually(|| rig
            .server
            .stats()
            .is_some_and(|stats| stats.producer_window_reserved == 0))
        .await,
        "windows are released when the streams end"
    );
    rig.stop();
}

/// A stream backlog to a peer that stopped reading is bounded by the
/// window reserved for it, and never makes the producer's own calls to
/// another peer fail `Overloaded`: stream frames do not count against the
/// request queue budgets.
async fn a_stream_backlog_does_not_starve_unrelated_calls() {
    let limits = ResourceLimits {
        max_queued_bytes_per_connection: 256 << 10,
        max_queued_bytes: 256 << 10,
        ..ResourceLimits::default()
    };
    let server_config = RpcConfig {
        limits,
        ..RpcConfig::default()
    };
    let rig = Rig::new(server_config, RpcConfig::default()).await;
    // Another runtime the producer calls.
    let (other_driver, other) =
        RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", RpcConfig::default())
            .await
            .expect("bind");
    let (echo_elsewhere, echoes) = other
        .register::<Echo>(AccessClass::Public)
        .expect("register");
    let echo_task = serve_echo(echoes);
    let other_driver = tokio::spawn(other_driver.run());
    // A raw peer opens a 16 MiB-window stream of 64 KiB items and never
    // reads a byte.
    let address = rig.server.address().expect("listening");
    let mut stalled = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let mut request = scan(100, 0, 64 * 1024);
    request.end = 3;
    stalled
        .write_all(&raw_stream_opening(&rig.scan, &request, 16 << 20))
        .await
        .expect("send");
    assert!(
        eventually(|| rig
            .server
            .stats()
            .is_some_and(|stats| stats.queued_bytes > 1 << 20))
        .await,
        "the producer's writer backed up behind the stalled reader"
    );
    let stats = rig.server.stats().expect("running");
    assert!(stats.queued_bytes <= (16 << 20) + (1 << 20), "{stats:?}");
    assert_eq!(stats.producer_window_reserved, 16 << 20);
    // The producer's own calls elsewhere are not refused.
    let caller = echo_elsewhere.bind(&rig.server);
    for id in 0..5 {
        let reply = caller
            .try_get_reply_within(&chunk(id, id, 1024), Duration::from_secs(5))
            .await
            .expect("an unrelated call succeeds beside the backlog");
        assert_eq!(reply.seq, id);
    }
    drop(stalled);
    echo_task.abort();
    other_driver.abort();
    rig.stop();
}
both_flavors!(
    a_stream_backlog_does_not_starve_unrelated_calls,
    stream_backlog_current_thread,
    stream_backlog_multi_thread
);
