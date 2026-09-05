//! End-to-end TCP flow control: one byte window per stream direction.
//!
//! The window is acquired when a write is accepted and released only when
//! the peer's application reads the bytes. Moving bytes from the send queue
//! onto the wire, or from the wire into the peer's receive buffer, releases
//! nothing: they are the same outstanding bytes in a different place. So a
//! reader that stops reading backs the writer up end to end, a direction
//! that stops delivering fills its window and blocks, and a partial read
//! returns exactly the credit it consumed.
//!
//! The first half drives a raw `SimWorld` and checks the accounting after
//! every event; the second half runs the metastability-shaped scenario (a
//! server streaming into a slow client, under latency, partial I/O and a
//! temporary partition) through `SimulationBuilder` with the determinism
//! canary armed.

use std::{
    future::Future,
    io,
    net::IpAddr,
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

use async_trait::async_trait;
use futures::{
    future::poll_fn,
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    task::noop_waker,
};
use moonpool_sim::{
    FaultContext, FaultInjector, LatencyDistribution, NetworkConfiguration, NetworkFaultMask,
    NetworkProvider, Process, SimContext, SimWorld, SimulationBuilder, SimulationReport,
    SimulationResult, TcpListenerTrait, TimeProvider, Workload, assert_always, assert_sometimes,
    buggify_reset, network::sim::SimTcpStream,
};

const MAX_DRIVER_STEPS: usize = 100_000;

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

fn poll_write_with(
    stream: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
    waker: &Waker,
) -> Poll<io::Result<usize>> {
    let mut context = Context::from_waker(waker);
    Pin::new(stream).poll_write(&mut context, data)
}

fn poll_write_once(stream: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> Poll<io::Result<usize>> {
    poll_write_with(stream, data, &noop_waker())
}

fn poll_read_once(
    stream: &mut (impl AsyncRead + Unpin),
    data: &mut [u8],
) -> Poll<io::Result<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_read(&mut context, data)
}

fn poll_close_once(stream: &mut (impl AsyncWrite + Unpin)) -> Poll<io::Result<()>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    Pin::new(stream).poll_close(&mut context)
}

struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn counting_waker() -> (Arc<WakeCounter>, Waker) {
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    (counter, waker)
}

fn config(window: usize, latency: Duration) -> NetworkConfiguration {
    let mut config = NetworkConfiguration::fast_local();
    config.tcp_send_window_bytes = window;
    config.write_latency = LatencyDistribution::Uniform {
        start: latency,
        end: latency,
    };
    config
}

fn connected(config: NetworkConfiguration) -> (SimWorld, SimTcpStream, SimTcpStream) {
    buggify_reset();
    let mut sim = SimWorld::new_with_network_config_and_seed(config, 20_260_905);
    let server_provider = sim.network_provider(server_ip());
    let listener = drive(&mut sim, server_provider.bind("10.0.1.2:8080")).expect("bind");
    let client_provider = sim.network_provider(client_ip());
    let client = drive(&mut sim, client_provider.connect("10.0.1.2:8080")).expect("connect");
    let (server, _) = drive(&mut sim, listener.accept()).expect("accept");
    sim.run_until_empty();
    (sim, client, server)
}

/// Write until the window refuses, returning how much it took.
fn fill_window(stream: &mut SimTcpStream, chunk: usize) -> usize {
    let payload = vec![0x5a; chunk];
    let mut written = 0;
    loop {
        match poll_write_once(stream, &payload) {
            Poll::Ready(Ok(n)) => written += n,
            Poll::Ready(Err(error)) => panic!("write failed: {error}"),
            Poll::Pending => return written,
        }
    }
}

/// The accounting identity every event must preserve when nothing vanished.
fn assert_accounted(sim: &SimWorld, sender: &SimTcpStream, receiver: &SimTcpStream) {
    let outstanding = sim.outstanding_send_bytes(sender.connection_id());
    assert!(outstanding <= sim.send_window_bytes(sender.connection_id()));
    assert_eq!(
        outstanding,
        sim.queued_send_bytes(sender.connection_id())
            + sim.in_flight_bytes(sender.connection_id())
            + sim.unread_bytes(receiver.connection_id()),
        "outstanding = queued + in flight + unread at the peer"
    );
}

/// The central test: a writer that never sees its bytes read fills the
/// window and parks, even once every byte sits in the peer's receive buffer;
/// the peer's read hands back exactly what it consumed and wakes the writer.
#[test]
fn a_reader_that_never_reads_parks_the_writer_at_the_window() {
    const WINDOW: usize = 64 * 1024;
    let (mut sim, mut client, mut server) = connected(config(WINDOW, Duration::from_micros(1)));
    assert_eq!(sim.send_window_bytes(client.connection_id()), WINDOW);

    assert_eq!(fill_window(&mut client, 8 * 1024), WINDOW);
    assert_eq!(sim.available_send_bytes(client.connection_id()), 0);

    // Everything reaches the peer's receive buffer and still counts.
    sim.run_until_empty();
    assert_eq!(sim.unread_bytes(server.connection_id()), WINDOW);
    assert_eq!(sim.outstanding_send_bytes(client.connection_id()), WINDOW);
    assert_accounted(&sim, &client, &server);
    let (wake_count, waker) = counting_waker();
    assert!(poll_write_with(&mut client, b"x", &waker).is_pending());

    // The reader takes 4 KiB: that, and only that, comes back.
    let mut buf = vec![0_u8; 4096];
    assert_eq!(
        poll_read_once(&mut server, &mut buf).map(|result| result.expect("read")),
        Poll::Ready(4096)
    );
    assert_eq!(
        wake_count.0.load(Ordering::Relaxed),
        1,
        "the parked writer is woken"
    );
    assert_eq!(sim.available_send_bytes(client.connection_id()), 4096);
    assert_eq!(
        poll_write_once(&mut client, &vec![0_u8; 8192]).map(|result| result.expect("write")),
        Poll::Ready(4096),
        "the next write accepts at most what was read"
    );
    assert_accounted(&sim, &client, &server);
}

#[test]
fn a_partial_read_releases_exactly_what_it_consumed() {
    const WINDOW: usize = 1000;
    let (mut sim, mut client, mut server) = connected(config(WINDOW, Duration::from_micros(1)));
    assert_eq!(fill_window(&mut client, 1000), WINDOW);
    sim.run_until_empty();

    let mut buf = vec![0_u8; 137];
    assert_eq!(
        poll_read_once(&mut server, &mut buf).map(|result| result.expect("read")),
        Poll::Ready(137)
    );
    assert_eq!(sim.available_send_bytes(client.connection_id()), 137);
    assert_eq!(
        sim.outstanding_send_bytes(client.connection_id()),
        WINDOW - 137
    );
    assert_accounted(&sim, &client, &server);
}

/// A held flight stays outstanding: no credit comes back for delay.
#[test]
fn a_partition_returns_no_credit_until_the_peer_reads() {
    const WINDOW: usize = 4096;
    const PAYLOAD: &[u8] = b"held-and-still-charged";
    let (mut sim, mut client, mut server) = connected(config(WINDOW, Duration::from_millis(10)));
    assert_eq!(
        poll_write_once(&mut client, PAYLOAD).map(|result| result.expect("write")),
        Poll::Ready(PAYLOAD.len())
    );
    assert!(sim.step(), "the chunk goes on the wire");
    sim.partition_pair(client_ip(), server_ip(), Duration::from_millis(200));
    while sim.is_partitioned(client_ip(), server_ip()) {
        assert!(sim.step());
        assert_eq!(
            sim.outstanding_send_bytes(client.connection_id()),
            PAYLOAD.len()
        );
        assert_accounted(&sim, &client, &server);
    }
    let mut received = vec![0_u8; PAYLOAD.len()];
    drive(&mut sim, server.read_exact(&mut received)).expect("the payload lands");
    assert_eq!(sim.outstanding_send_bytes(client.connection_id()), 0);
    assert_eq!(sim.available_send_bytes(client.connection_id()), WINDOW);
}

/// A black-holed direction accepts a window's worth and then blocks: a small
/// RPC still goes in and times out at the application, a bulk transfer stops.
#[test]
fn a_black_hole_fills_its_window_and_then_applies_backpressure() {
    const WINDOW: usize = 1024;
    let (mut sim, mut client, mut server) = connected(config(WINDOW, Duration::from_micros(1)));
    sim.black_hole_connection(client.connection_id(), true, false);

    // The small request is accepted as before...
    assert_eq!(
        poll_write_once(&mut client, &[7; 100]).map(|result| result.expect("write")),
        Poll::Ready(100)
    );
    sim.run_until_empty();
    let mut buf = [0_u8; 256];
    assert!(poll_read_once(&mut server, &mut buf).is_pending());
    assert_eq!(
        sim.outstanding_send_bytes(client.connection_id()),
        100,
        "swallowed bytes stay charged"
    );

    // ...but the direction cannot absorb data forever.
    assert_eq!(fill_window(&mut client, 4096), WINDOW - 100);
    sim.run_until_empty();
    let (wake_count, waker) = counting_waker();
    assert!(poll_write_with(&mut client, b"x", &waker).is_pending());
    assert_eq!(sim.outstanding_send_bytes(client.connection_id()), WINDOW);
    assert_eq!(
        wake_count.0.load(Ordering::Relaxed),
        0,
        "nothing will ever wake it"
    );
    assert!(poll_read_once(&mut server, &mut buf).is_pending());
}

/// An abort wakes a parked writer to the error instead of leaving it on
/// credits the connection can no longer return.
#[test]
fn an_abort_wakes_a_writer_parked_on_the_window() {
    const WINDOW: usize = 1024;
    let (mut sim, mut client, server) = connected(config(WINDOW, Duration::from_micros(1)));
    assert_eq!(fill_window(&mut client, 4096), WINDOW);
    sim.run_until_empty();
    let (wake_count, waker) = counting_waker();
    assert!(poll_write_with(&mut client, b"x", &waker).is_pending());

    sim.close_connection_abort(server.connection_id());

    assert_eq!(wake_count.0.load(Ordering::Relaxed), 1);
    assert!(
        matches!(poll_write_once(&mut client, b"x"), Poll::Ready(Err(_))),
        "the woken writer observes the connection error, not another Pending"
    );
    assert_eq!(
        sim.close_reason(client.connection_id()),
        moonpool_sim::network::sim::CloseReason::Aborted
    );
}

/// A peer that closes with the window's bytes unread frees them: the writer
/// is woken and, on its next write, told the stream is gone.
#[test]
fn a_peer_close_with_unread_bytes_frees_the_window() {
    const WINDOW: usize = 1024;
    let (mut sim, mut client, mut server) = connected(config(WINDOW, Duration::from_micros(1)));
    assert_eq!(fill_window(&mut client, 4096), WINDOW);
    sim.run_until_empty();
    assert_eq!(sim.unread_bytes(server.connection_id()), WINDOW);
    let (wake_count, waker) = counting_waker();
    assert!(poll_write_with(&mut client, b"x", &waker).is_pending());

    assert!(matches!(poll_close_once(&mut server), Poll::Ready(Ok(()))));

    assert_eq!(
        wake_count.0.load(Ordering::Relaxed),
        1,
        "the writer is woken"
    );
    assert_eq!(sim.outstanding_send_bytes(client.connection_id()), 0);
    assert_eq!(
        poll_write_once(&mut client, b"after-close").map(|result| result.expect("write")),
        Poll::Ready(11),
        "writes into a closed peer are accepted, as before, and discarded"
    );
    sim.run_until_empty();
    assert_eq!(
        sim.outstanding_send_bytes(client.connection_id()),
        0,
        "what a closed peer discards on arrival is released too"
    );
    let mut byte = [0_u8; 1];
    assert_eq!(
        drive(&mut sim, client.read(&mut byte)).expect("read"),
        0,
        "the FIN arrives"
    );
}

/// The FIN goes behind the last byte, even when the window held that byte
/// back in the send queue.
#[test]
fn the_fin_never_overtakes_outstanding_data() {
    const WINDOW: usize = 512;
    let (mut sim, mut client, mut server) = connected(config(WINDOW, Duration::from_micros(1)));
    let payload = (0..WINDOW)
        .map(|i| u8::try_from(i % 251).expect("small"))
        .collect::<Vec<_>>();
    assert_eq!(
        poll_write_once(&mut client, &payload).map(|result| result.expect("write")),
        Poll::Ready(WINDOW)
    );
    assert!(matches!(poll_close_once(&mut client), Poll::Ready(Ok(()))));

    let mut received = vec![0_u8; WINDOW];
    drive(&mut sim, server.read_exact(&mut received)).expect("every byte lands");
    assert_eq!(received, payload);
    let mut byte = [0_u8; 1];
    assert_eq!(drive(&mut sim, server.read(&mut byte)).expect("read"), 0);
}

/// The budget stays bounded and exactly accounted through a whole scripted
/// run: large latency, a slow reader taking small bites, a temporary cut.
#[test]
fn the_outstanding_budget_stays_bounded_under_latency_and_a_cut() {
    const WINDOW: usize = 2048;
    const TOTAL: usize = 16 * 1024;
    let (mut sim, mut client, mut server) = connected(config(WINDOW, Duration::from_millis(50)));
    let payload = (0..TOTAL)
        .map(|i| u8::try_from(i % 253).expect("small"))
        .collect::<Vec<_>>();
    let mut written = 0;
    let mut received = Vec::with_capacity(TOTAL);
    let mut max_outstanding = 0;
    let mut cut = false;
    let mut steps = 0;
    while received.len() < TOTAL {
        if written < TOTAL
            && let Poll::Ready(Ok(n)) = poll_write_once(&mut client, &payload[written..])
        {
            written += n;
        }
        // The reader takes small bites, and only every other event.
        if steps % 2 == 0 {
            let mut buf = [0_u8; 300];
            if let Poll::Ready(Ok(n)) = poll_read_once(&mut server, &mut buf) {
                received.extend_from_slice(&buf[..n]);
            }
        }
        if !cut && received.len() > TOTAL / 4 {
            sim.partition_pair(client_ip(), server_ip(), Duration::from_millis(300));
            cut = true;
        }
        assert_accounted(&sim, &client, &server);
        max_outstanding = max_outstanding.max(sim.outstanding_send_bytes(client.connection_id()));
        if sim.has_pending_events() {
            sim.step();
        }
        steps += 1;
        assert!(steps < MAX_DRIVER_STEPS, "the transfer stalled");
    }
    assert_eq!(received, payload, "the stream arrives intact and in order");
    assert_eq!(max_outstanding, WINDOW, "the writer ran up to the window");
    assert_eq!(sim.outstanding_send_bytes(client.connection_id()), 0);
}

// ---------------------------------------------------------------------------
// Campaign: a server streaming into a slow client.
// ---------------------------------------------------------------------------

/// Bytes the server streams over one connection.
const STREAM_LEN: usize = 48 * 1024;
/// Window small enough that the streamer blocks many times per run.
const CAMPAIGN_WINDOW: usize = 4096;
const SERVER_PORT: u16 = 9000;

fn expected_byte(index: usize) -> u8 {
    u8::try_from(index % 241).expect("small")
}

/// Streams `STREAM_LEN` patterned bytes to every client, counting how often
/// the window parked it.
struct Streamer;

#[async_trait]
impl Process for Streamer {
    fn name(&self) -> &'static str {
        "streamer"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx
            .network()
            .bind(&format!("{}:{SERVER_PORT}", ctx.my_ip()))
            .await?;
        loop {
            let accepted = moonpool_sim::select! {
                biased;
                r = listener.accept() => r,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };
            let Ok((mut stream, _)) = accepted else {
                continue;
            };
            let payload = (0..STREAM_LEN).map(expected_byte).collect::<Vec<_>>();
            let mut written = 0;
            let mut parked = 0_u64;
            while written < payload.len() {
                let result =
                    poll_fn(
                        |cx| match Pin::new(&mut stream).poll_write(cx, &payload[written..]) {
                            Poll::Pending => {
                                parked += 1;
                                Poll::Pending
                            }
                            ready @ Poll::Ready(_) => ready,
                        },
                    )
                    .await;
                match result {
                    Ok(n) => written += n,
                    Err(_) => break,
                }
            }
            assert_sometimes!(
                parked > 0,
                "flow control: the streamer parked on a full window"
            );
            assert_always!(
                written <= payload.len(),
                "flow control: the streamer never writes past its payload"
            );
        }
    }
}

/// Reads the stream in small bites with a pause between them, checking the
/// pattern as it goes.
struct SlowReader;

#[async_trait]
impl Workload for SlowReader {
    fn name(&self) -> &'static str {
        "slow-reader"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server = ctx.topology().all_process_ips()[0].clone();
        let mut stream = ctx
            .network()
            .connect(&format!("{server}:{SERVER_PORT}"))
            .await?;
        let mut received = 0;
        let mut buf = [0_u8; 256];
        while received < STREAM_LEN {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            for (offset, byte) in buf[..n].iter().enumerate() {
                assert_always!(
                    *byte == expected_byte(received + offset),
                    "flow control: the slow reader sees the stream in order"
                );
            }
            received += n;
            let _ = ctx.time().sleep(Duration::from_millis(1)).await;
        }
        assert_always!(
            received == STREAM_LEN,
            "flow control: the slow reader receives the whole stream"
        );
        Ok(())
    }
}

/// Cuts the client from the server for a while, twice, then stays quiet.
struct TemporaryCut;

#[async_trait]
impl FaultInjector for TemporaryCut {
    fn name(&self) -> &'static str {
        "temporary-cut"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let server = ctx.process_ips()[0].clone();
        let client = "10.0.0.1";
        for _ in 0..2 {
            moonpool_sim::select! {
                biased;
                () = ctx.chaos_shutdown().cancelled() => return Ok(()),
                _ = ctx.time().sleep(Duration::from_millis(40)) => {}
            }
            ctx.partition(client, &server)?;
            moonpool_sim::select! {
                biased;
                () = ctx.chaos_shutdown().cancelled() => {}
                _ = ctx.time().sleep(Duration::from_millis(60)) => {}
            }
            ctx.heal_partition(client, &server)?;
        }
        Ok(())
    }
}

/// Latency, partial writes and reads (buggify is live in a campaign), the
/// scripted cut, and nothing that could end the stream on its own: a random
/// close or a hung connect would turn a flow-control run into a different
/// test, and a bit flip would break the pattern check for a reason that is
/// not the window.
fn campaign() -> SimulationBuilder {
    SimulationBuilder::new()
        .processes(1, || Box::new(Streamer))
        .workload_factory(|| Box::new(SlowReader))
        .fault_factory(|| Box::new(TemporaryCut))
        .network_fault_mask(NetworkFaultMask::none())
        .tcp_send_window_bytes(CAMPAIGN_WINDOW)
        .chaos_duration(Duration::from_secs(2))
}

fn sometimes_pass_count(report: &SimulationReport, message: &str) -> u64 {
    report
        .assertion_details
        .iter()
        .find(|detail| detail.msg == message)
        .map_or(0, |detail| detail.pass_count)
}

/// The metastability shape: the streamer fills the window, parks, is woken
/// by the slow reader, resumes — through latency, partial writes and reads,
/// and a temporary cut — and every seed replays draw for draw.
#[test]
fn a_streamer_into_a_slow_reader_blocks_resumes_and_replays_deterministically() {
    let report = campaign()
        .check_determinism()
        .set_iterations(4)
        .set_debug_seeds(vec![1, 2, 3, 4])
        .run();

    assert_eq!(
        report.failed_runs,
        0,
        "{:?} {:?} {:?}",
        report.assertion_violations,
        report.seeds_failing,
        report
            .individual_metrics
            .iter()
            .filter_map(|m| m.as_ref().err())
            .collect::<Vec<_>>()
    );
    assert_eq!(report.iterations, 4);
    assert_eq!(
        sometimes_pass_count(
            &report,
            "flow control: the streamer parked on a full window"
        ),
        8,
        "the window parked the streamer on every seed, on the record run and the replay"
    );
    assert_eq!(
        sometimes_pass_count(
            &report,
            "determinism canary: replay matched the recorded draw sequence"
        ),
        4,
        "one canary verdict per seed"
    );
}
