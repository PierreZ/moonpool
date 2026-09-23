//! The consumer: opens reply streams against the producer and consumes
//! them at every pace — at once, slowly, not at all — abandons them before
//! and after their first item, times out on them, saturates their credit
//! while probing that control and unrelated calls still progress, asks for
//! producer reboots mid-stream, and judges every stream against the
//! producer and consumer ledgers.

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use moonpool_rpc::protocol::stream_item_frame_len;
use moonpool_rpc::{
    AccessClass, BootstrapAddress, ErrorReason, Execution, ReplyStream, RpcDriver, RpcError,
    RpcHandle, ServiceRef, WellKnownRef,
};
use moonpool_sim::{
    RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult, TimeProvider,
    Workload, assert_always, assert_reachable, assert_sometimes,
};

use super::messages::{
    DIRECTORY_ID, Directory, End, Listing, Lookup, Ping, Probe, SHUTDOWN_CODE, Scan, ScanItems,
};
use super::policy::streams_config;
use super::state::{
    FAULTS_DONE_KEY, PRODUCER_BOOTS_KEY, PRODUCER_LABEL, ProducerEnd, REBOOT_REQUESTS_KEY,
    StreamLedger, WORKLOAD_LABEL,
};
use super::{RPC_PORT, StreamsRecord, StreamsRecords, pause, report_stats};
use crate::foundations::state::Board;

/// One operation of the workload's alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StreamOp {
    /// Open a stream and consume it to its end at a random pace.
    Complete,
    /// A slow reader with a window of a few items.
    SlowReader,
    /// A slow writer: the reader is always waiting.
    SlowWriter,
    /// Abandon the stream before its first item.
    AbandonBefore,
    /// Abandon the stream after its first items.
    AbandonAfter,
    /// Give up on an item that takes too long, abandoning the stream.
    Timeout,
    /// The producer fills the window, then fails; the reader only reads
    /// afterwards.
    FailExhausted,
    /// The producer drops its stream: a broken promise.
    Broken,
    /// The producer's item is larger than the window.
    Oversize,
    /// Several streams exhaust their credit; unary calls, a new stream and
    /// a cancel must still get through.
    Saturate,
    /// Many calls and streams at once, against squeezed budgets.
    Burst,
    /// Ask for a producer crash or graceful shutdown mid-stream.
    Reboot,
}

impl StreamOp {
    const ALL: [Self; 12] = [
        Self::Complete,
        Self::SlowReader,
        Self::SlowWriter,
        Self::AbandonBefore,
        Self::AbandonAfter,
        Self::Timeout,
        Self::FailExhausted,
        Self::Broken,
        Self::Oversize,
        Self::Saturate,
        Self::Burst,
        Self::Reboot,
    ];
}

/// The workload's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamsConfig {
    /// Operations per run.
    pub operations: usize,
    /// Relative weight per [`StreamOp`], in declaration order.
    pub weights: [u32; 12],
    /// Pause between operations, in milliseconds (half-open range).
    pub gap_ms: (u64, u64),
}

impl StreamsConfig {
    /// The full campaign mix.
    #[must_use]
    pub fn campaign() -> Self {
        Self {
            operations: 28,
            weights: [12, 7, 6, 6, 6, 5, 6, 4, 3, 5, 4, 4],
            gap_ms: (5, 120),
        }
    }
}

/// How a stream ended for the workload's application.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// `None` after the items: a normal end.
    Normal,
    /// A terminal error after the items.
    Failed(RpcError),
    /// The workload dropped the stream.
    Abandoned,
    /// Opening it failed before anything was queued.
    NotOpened(RpcError),
}

/// One judged stream.
struct Judged {
    id: u64,
    op: StreamOp,
    outcome: Outcome,
}

/// A probe refused before admission, to judge against the probe ledger.
struct RefusedProbe {
    id: u64,
}

/// How the reader paces itself.
#[derive(Debug, Clone, Copy)]
enum Pace {
    /// Take items as they come.
    Eager,
    /// Pause this long after each item.
    Slow(Duration),
}

/// Where a bounded read stopped.
enum Read {
    /// The stream ended (`None`: normally).
    Ended(Option<RpcError>),
    /// `limit` items were taken.
    Limit,
    /// An item did not come within the patience.
    Timeout,
}

/// Deadline for things that should complete.
const LONG: Duration = Duration::from_secs(25);

/// How long a reboot request may take to end its stream before the
/// workload gives up on it (the script may have stopped serving).
const REBOOT_PATIENCE: Duration = Duration::from_secs(10);

/// How long the producer may take to release streams whose consumer is
/// gone. A consumer that gave up during a partition may leave the
/// producer's side of the session half open; the producer then notices
/// only after its inbound idle timeout plus a probe (at most 36 s + 6 s
/// with the campaign's knobs, plus ping-loop jitter).
const RELEASE_BOUND: Duration = Duration::from_mins(1);

/// The campaign's workload.
pub struct StreamsWorkload {
    config: StreamsConfig,
    records: StreamsRecords,
    history: Vec<String>,
    judged: Vec<Judged>,
    refused_probes: Vec<RefusedProbe>,
    next_id: u64,
    listing: Option<Listing>,
    stale: bool,
    ledger: StreamLedger,
}

type Streams = ServiceRef<ScanItems>;

impl StreamsWorkload {
    /// A fresh workload appending its run record to `records`.
    #[must_use]
    pub fn new(config: StreamsConfig, records: StreamsRecords) -> Self {
        Self {
            config,
            records,
            history: Vec::new(),
            judged: Vec::new(),
            refused_probes: Vec::new(),
            next_id: 0,
            listing: None,
            stale: true,
            ledger: StreamLedger::default(),
        }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn pick(&self, ctx: &SimContext) -> StreamOp {
        let total: u32 = self.config.weights.iter().sum();
        let mut draw = ctx.random().random_range(0..total.max(1));
        for (op, weight) in StreamOp::ALL.into_iter().zip(self.config.weights) {
            if draw < weight {
                return op;
            }
            draw -= weight;
        }
        StreamOp::Complete
    }

    fn judge(&mut self, id: u64, op: StreamOp, outcome: Outcome) {
        let line = match &outcome {
            Outcome::Normal => "normal".to_string(),
            Outcome::Abandoned => "abandoned".to_string(),
            Outcome::Failed(error) => {
                format!("failed {:?}/{:?}", error.reason(), error.execution())
            }
            Outcome::NotOpened(error) => {
                format!("not-opened {:?}/{:?}", error.reason(), error.execution())
            }
        };
        let taken = self.ledger.consumed(id).items.len();
        self.history.push(format!("{id} {op:?} {taken} {line}"));
        if let Outcome::Failed(error) | Outcome::NotOpened(error) = &outcome
            && matches!(
                error.reason(),
                ErrorReason::StaleIncarnation
                    | ErrorReason::EndpointNotFound
                    | ErrorReason::Disconnected
                    | ErrorReason::ConnectFailed(_)
            )
        {
            self.stale = true;
        }
        self.judged.push(Judged { id, op, outcome });
    }

    /// Learn the producer's current references through its well-known
    /// directory, retrying (a directory read is idempotent).
    async fn discover(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        producer: SocketAddr,
    ) -> Option<(Streams, ServiceRef<Ping>)> {
        if !self.stale
            && let Some(listing) = &self.listing
        {
            return refs(listing);
        }
        let directory = WellKnownRef::<Directory>::new(
            BootstrapAddress::Resolved(producer),
            DIRECTORY_ID,
            AccessClass::Public,
        )
        .at(producer)
        .bind(rpc);
        for _ in 0..40 {
            match directory
                .try_get_reply_within(&Lookup {}, Duration::from_secs(2))
                .await
            {
                Ok(listing) => {
                    self.history.push(format!("discover boot {}", listing.boot));
                    self.stale = false;
                    let found = refs(&listing);
                    self.listing = Some(listing);
                    return found;
                }
                Err(_) => {
                    if pause(ctx, Duration::from_millis(100)).await.is_err() {
                        return None;
                    }
                }
            }
        }
        self.history.push("discover failed".to_string());
        None
    }

    /// Open stream `id` with `window`, judging a refusal.
    fn open(
        &mut self,
        streams: &Streams,
        rpc: &RpcHandle<SimProviders>,
        scan: &Scan,
        window: u64,
        op: StreamOp,
    ) -> Option<ReplyStream<ScanItems>> {
        match streams.bind(rpc).get_reply_stream_with_window(scan, window) {
            Ok(stream) => Some(stream),
            Err(error) => {
                assert_always!(
                    error.execution() == Execution::NotAdmitted,
                    "a stream that failed to open was never admitted"
                );
                self.judge(scan.id, op, Outcome::NotOpened(error));
                None
            }
        }
    }

    /// Take items from `stream` into the consumer ledger, checking order
    /// and the buffer bound, until it ends, `limit` items were taken, or
    /// an item takes longer than `patience`.
    async fn read(
        &self,
        ctx: &SimContext,
        stream: &mut ReplyStream<ScanItems>,
        id: u64,
        pace: Pace,
        limit: Option<usize>,
        patience: Duration,
    ) -> Read {
        let mut taken = self.ledger.consumed(id).items.len();
        loop {
            if limit.is_some_and(|limit| taken >= limit) {
                return Read::Limit;
            }
            let Ok(next) = ctx.time().timeout(patience, stream.next()).await else {
                return Read::Timeout;
            };
            match next {
                None => return Read::Ended(None),
                Some(Err(error)) => {
                    assert_always!(
                        stream.next().await.is_none(),
                        "a stream has exactly one terminal outcome"
                    );
                    return Read::Ended(Some(error));
                }
                Some(Ok(chunk)) => {
                    let expected = u64::try_from(taken).unwrap_or(u64::MAX);
                    assert_always!(
                        chunk.id == id && chunk.seq == expected,
                        "stream items arrive in order, without gaps or repeats"
                    );
                    assert_always!(
                        stream.buffered_bytes() <= stream.window(),
                        "a consumer never buffers more than its window"
                    );
                    let size = stream_item_frame_len(prost::Message::encoded_len(&chunk));
                    self.ledger.consume(id, chunk.seq, size, chunk.boot);
                    taken += 1;
                    if let Pace::Slow(delay) = pace
                        && pause(ctx, delay).await.is_err()
                    {
                        return Read::Timeout;
                    }
                }
            }
        }
    }

    fn scan(&mut self, count: u64, item_bytes: u32, end: End) -> Scan {
        Scan {
            id: self.id(),
            count,
            item_bytes,
            first_delay_ms: 0,
            delay_ms: 0,
            end: end as u32,
            code: 0,
        }
    }

    /// A window of `items` items of `item_bytes` payload (an upper bound
    /// on each item's accounted size).
    fn window(item_bytes: u32, items: u64) -> u64 {
        stream_item_frame_len(item_bytes as usize + 40) * items.max(1)
    }

    /// Consume a whole stream and judge how it ended.
    async fn drain(
        &mut self,
        ctx: &SimContext,
        mut stream: ReplyStream<ScanItems>,
        id: u64,
        op: StreamOp,
        pace: Pace,
    ) -> Outcome {
        let outcome = match self.read(ctx, &mut stream, id, pace, None, LONG).await {
            Read::Ended(None) => Outcome::Normal,
            Read::Ended(Some(error)) => Outcome::Failed(error),
            Read::Limit | Read::Timeout => {
                drop(stream);
                Outcome::Abandoned
            }
        };
        self.judge(id, op, outcome.clone());
        outcome
    }

    async fn step(
        &mut self,
        op: StreamOp,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        streams: &Streams,
        ping: &ServiceRef<Ping>,
    ) {
        let random = ctx.random();
        match op {
            StreamOp::Complete | StreamOp::SlowReader | StreamOp::SlowWriter => {
                self.paced(op, ctx, rpc, streams).await;
            }
            StreamOp::AbandonBefore => {
                let mut scan = self.scan(0, 64, End::Endless);
                scan.first_delay_ms = random.random_range(50..400);
                let hold = Duration::from_millis(random.random_range(0..40));
                if let Some(stream) = self.open(streams, rpc, &scan, Self::window(64, 4), op) {
                    if pause(ctx, hold).await.is_err() {
                        return;
                    }
                    if stream.received() == 0 {
                        assert_reachable!("rpc stream abandoned before its first item");
                    }
                    drop(stream);
                    self.judge(scan.id, op, Outcome::Abandoned);
                }
            }
            StreamOp::AbandonAfter => {
                let mut scan = self.scan(0, random.random_range(0..500), End::Endless);
                scan.delay_ms = random.random_range(0..10);
                let limit = random.random_range(1..5);
                let window = Self::window(scan.item_bytes, random.random_range(1..6));
                if let Some(mut stream) = self.open(streams, rpc, &scan, window, op) {
                    let read = self
                        .read(ctx, &mut stream, scan.id, Pace::Eager, Some(limit), LONG)
                        .await;
                    self.abandon_or_judge(read, stream, scan.id, op, || {
                        assert_reachable!("rpc stream abandoned after its first item");
                    });
                }
            }
            StreamOp::Timeout => {
                let mut scan = self.scan(0, 32, End::Endless);
                scan.delay_ms = random.random_range(100..300);
                let patience = Duration::from_millis(random.random_range(10..90));
                if let Some(mut stream) = self.open(streams, rpc, &scan, Self::window(32, 4), op) {
                    // The first item comes at once; the next one is late.
                    let read = self
                        .read(ctx, &mut stream, scan.id, Pace::Eager, Some(1), LONG)
                        .await;
                    let read = match read {
                        Read::Limit => {
                            self.read(ctx, &mut stream, scan.id, Pace::Eager, None, patience)
                                .await
                        }
                        other => other,
                    };
                    self.abandon_or_judge(read, stream, scan.id, op, || {
                        assert_reachable!("rpc stream caller timed out and abandoned the stream");
                    });
                }
            }
            StreamOp::FailExhausted => self.fail_exhausted(ctx, rpc, streams).await,
            StreamOp::Broken => {
                let scan = self.scan(random.random_range(0..4), 16, End::Drop);
                if let Some(stream) = self.open(streams, rpc, &scan, Self::window(16, 4), op)
                    && let Outcome::Failed(error) =
                        self.drain(ctx, stream, scan.id, op, Pace::Eager).await
                    && *error.reason() == ErrorReason::BrokenPromise
                {
                    assert_sometimes!(true, "rpc stream broken promise reported after its items");
                }
            }
            StreamOp::Oversize => {
                let scan = self.scan(3, random.random_range(600..3000), End::Finish);
                // Room for small items only: the producer's first item can
                // never be sent.
                let window = stream_item_frame_len(256);
                if let Some(stream) = self.open(streams, rpc, &scan, window, op) {
                    let _ = self.drain(ctx, stream, scan.id, op, Pace::Eager).await;
                }
            }
            StreamOp::Saturate => self.saturate(ctx, rpc, streams, ping).await,
            StreamOp::Burst => self.burst(ctx, rpc, streams, ping).await,
            StreamOp::Reboot => self.reboot(ctx, rpc, streams).await,
        }
    }

    /// Streams consumed to their end: at a random pace, by a slow reader
    /// with a window of a few items, or from a slow writer.
    async fn paced(
        &mut self,
        op: StreamOp,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        streams: &Streams,
    ) {
        let random = ctx.random();
        match op {
            StreamOp::Complete => {
                let item = random.random_range(0..1500);
                let scan = self.scan(random.random_range(0..25), item, End::Finish);
                let window = Self::window(item, random.random_range(1..8));
                let pace = if random.random_bool(0.5) {
                    Pace::Eager
                } else {
                    Pace::Slow(Duration::from_millis(random.random_range(1..15)))
                };
                if let Some(stream) = self.open(streams, rpc, &scan, window, op)
                    && self.drain(ctx, stream, scan.id, op, pace).await == Outcome::Normal
                {
                    assert_sometimes!(true, "rpc stream completed normally with every item");
                }
            }
            StreamOp::SlowReader => {
                let item = random.random_range(100..1500);
                let scan = self.scan(random.random_range(8..30), item, End::Finish);
                let window = Self::window(item, random.random_range(1..3));
                let pace = Pace::Slow(Duration::from_millis(random.random_range(5..40)));
                if let Some(stream) = self.open(streams, rpc, &scan, window, op) {
                    let _ = self.drain(ctx, stream, scan.id, op, pace).await;
                }
            }
            _ => {
                let mut scan = self.scan(random.random_range(2..8), 64, End::Finish);
                scan.delay_ms = random.random_range(5..40);
                if let Some(stream) = self.open(streams, rpc, &scan, Self::window(64, 4), op) {
                    let _ = self.drain(ctx, stream, scan.id, op, Pace::Eager).await;
                }
            }
        }
    }

    /// Judge a bounded read: an end is judged as such; otherwise the
    /// stream is abandoned, and `abandoned` runs.
    fn abandon_or_judge(
        &mut self,
        read: Read,
        stream: ReplyStream<ScanItems>,
        id: u64,
        op: StreamOp,
        abandoned: impl FnOnce(),
    ) {
        match read {
            Read::Ended(None) => self.judge(id, op, Outcome::Normal),
            Read::Ended(Some(error)) => self.judge(id, op, Outcome::Failed(error)),
            Read::Limit | Read::Timeout => {
                if stream.received() > 0 {
                    abandoned();
                }
                drop(stream);
                self.judge(id, op, Outcome::Abandoned);
            }
        }
    }

    /// The producer fills the window and fails; the reader reads only
    /// after the failure was recorded: the error must arrive, after every
    /// item, although no credit was left.
    async fn fail_exhausted(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        streams: &Streams,
    ) {
        let op = StreamOp::FailExhausted;
        let items = ctx.random().random_range(1..5);
        let item = ctx.random().random_range(16..800);
        let mut scan = self.scan(items, item, End::Fail);
        scan.code = ctx.random().random_range(1..1000);
        let window = Self::window(item, items);
        let Some(stream) = self.open(streams, rpc, &scan, window, op) else {
            return;
        };
        for _ in 0..200 {
            if self
                .ledger
                .produced(scan.id)
                .is_some_and(|produced| produced.end.is_some())
            {
                break;
            }
            if pause(ctx, Duration::from_millis(10)).await.is_err() {
                return;
            }
        }
        let exhausted = self.ledger.produced(scan.id).is_some_and(|produced| {
            produced.end == Some(ProducerEnd::Failed(scan.code))
                && produced.in_flight_at_end + stream_item_frame_len(item as usize) > window
        });
        if let Outcome::Failed(error) = self.drain(ctx, stream, scan.id, op, Pace::Eager).await
            && error.reason() == &(ErrorReason::StreamFailed { code: scan.code })
            && exhausted
        {
            assert_sometimes!(true, "rpc stream error delivered under exhausted credit");
        }
    }

    /// Several endless streams exhaust their credit (nobody reads); a
    /// unary call, a new stream and a cancel must still get through, and
    /// the producers resume once reading does.
    async fn saturate(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        streams: &Streams,
        ping: &ServiceRef<Ping>,
    ) {
        let op = StreamOp::Saturate;
        let count = ctx.random().random_range(2..5);
        let item = ctx.random().random_range(1000..6000);
        let mut open = Vec::new();
        for _ in 0..count {
            let scan = self.scan(0, item, End::Endless);
            let window = Self::window(item, ctx.random().random_range(2..6));
            if let Some(stream) = self.open(streams, rpc, &scan, window, op) {
                open.push((scan.id, stream));
            }
        }
        if pause(
            ctx,
            Duration::from_millis(ctx.random().random_range(100..300)),
        )
        .await
        .is_err()
        {
            return;
        }
        let saturated = open.iter().all(|(id, stream)| {
            self.ledger.produced(*id).is_some_and(|produced| {
                produced.end.is_none()
                    && produced.sent_bytes + stream_item_frame_len(item as usize + 40)
                        > stream.window()
            })
        });
        // A unary call beside the saturated streams.
        let probe = Probe {
            id: self.id(),
            hold_ms: 0,
        };
        let reply = ping
            .bind(rpc)
            .try_get_reply_within(&probe, Duration::from_secs(2))
            .await;
        if saturated && reply.is_ok() {
            assert_sometimes!(
                true,
                "rpc unary call progressed while streams saturated their credit"
            );
        }
        // A new stream beside them.
        let fresh = self.scan(3, 32, End::Finish);
        if let Some(stream) = self.open(streams, rpc, &fresh, Self::window(32, 4), op)
            && self.drain(ctx, stream, fresh.id, op, Pace::Eager).await == Outcome::Normal
            && saturated
        {
            assert_sometimes!(true, "rpc new stream progressed beside saturated ones");
        }
        // A cancel beside them: its producer must stop.
        if let Some((id, stream)) = open.pop() {
            drop(stream);
            self.judge(id, op, Outcome::Abandoned);
            for _ in 0..200 {
                if self
                    .ledger
                    .produced(id)
                    .is_some_and(|produced| produced.end.is_some())
                {
                    if saturated {
                        assert_sometimes!(
                            true,
                            "rpc stream cancel took effect while the session was saturated"
                        );
                    }
                    break;
                }
                if pause(ctx, Duration::from_millis(10)).await.is_err() {
                    return;
                }
            }
        }
        // Reading resumes: the blocked producers wake and send more.
        for (id, mut stream) in open {
            let before = self
                .ledger
                .produced(id)
                .map_or(0, |produced| produced.items.len());
            let taken = self.ledger.consumed(id).items.len();
            let read = self
                .read(ctx, &mut stream, id, Pace::Eager, Some(before + 2), LONG)
                .await;
            let after = self
                .ledger
                .produced(id)
                .map_or(0, |produced| produced.items.len());
            if matches!(read, Read::Limit) && after > before && taken < before {
                assert_sometimes!(true, "rpc stream producer resumed after consumption");
            }
            self.abandon_or_judge(read, stream, id, op, || {});
        }
    }

    /// Many unary calls and streams at once: against squeezed budgets some
    /// are refused before admission, and a refused call never ran.
    async fn burst(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        streams: &Streams,
        ping: &ServiceRef<Ping>,
    ) {
        let op = StreamOp::Burst;
        let calls = ctx.random().random_range(6..28);
        let hold = ctx.random().random_range(10..60);
        let client = ping.bind(rpc);
        let probes: Vec<Probe> = (0..calls)
            .map(|_| Probe {
                id: self.id(),
                hold_ms: hold,
            })
            .collect();
        let replies = futures::future::join_all(
            probes
                .iter()
                .map(|probe| client.try_get_reply_within(probe, Duration::from_secs(3))),
        )
        .await;
        for (probe, reply) in probes.iter().zip(replies) {
            if let Err(error) = reply
                && *error.reason() == ErrorReason::Overloaded
            {
                assert_always!(
                    error.execution() == Execution::NotAdmitted,
                    "an overload refusal proves the request was not admitted"
                );
                assert_sometimes!(true, "rpc request refused overloaded before admission");
                self.refused_probes.push(RefusedProbe { id: probe.id });
            }
        }
        let count = ctx.random().random_range(2..12);
        let mut open = Vec::new();
        for _ in 0..count {
            let mut scan = self.scan(2, 16, End::Finish);
            // Held open a while, so the burst's streams overlap.
            scan.first_delay_ms = ctx.random().random_range(20..60);
            if let Some(stream) = self.open(streams, rpc, &scan, Self::window(16, 2), op) {
                open.push((scan.id, stream));
            }
        }
        for (id, stream) in open {
            if let Outcome::Failed(error) = self.drain(ctx, stream, id, op, Pace::Eager).await
                && *error.reason() == ErrorReason::Overloaded
            {
                assert_always!(
                    error.execution() == Execution::NotAdmitted,
                    "an overloaded stream was refused before admission"
                );
                assert_sometimes!(true, "rpc stream refused overloaded before admission");
            }
        }
    }

    /// Ask the fault script to crash or gracefully stop the producer while
    /// a stream runs, and read the stream to its end.
    async fn reboot(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>, streams: &Streams) {
        let op = StreamOp::Reboot;
        let graceful = ctx.random().random_bool(0.5);
        let mut scan = self.scan(0, 64, End::Endless);
        scan.delay_ms = ctx.random().random_range(5..30);
        let Some(mut stream) = self.open(streams, rpc, &scan, Self::window(64, 4), op) else {
            return;
        };
        let read = self
            .read(ctx, &mut stream, scan.id, Pace::Eager, Some(1), LONG)
            .await;
        if !matches!(read, Read::Limit) {
            self.abandon_or_judge(read, stream, scan.id, op, || {});
            return;
        }
        let mut requests = ctx
            .state()
            .get::<Vec<bool>>(REBOOT_REQUESTS_KEY)
            .unwrap_or_default();
        requests.push(graceful);
        ctx.state().publish(REBOOT_REQUESTS_KEY, requests);
        // Read until the reboot ends the stream. A request that raced the
        // end of the fault script is never served: give up after a bound
        // and abandon the stream instead of reading an endless one forever.
        let deadline = ctx.time().now() + REBOOT_PATIENCE;
        let read = loop {
            let limit = self.ledger.consumed(scan.id).items.len() + 16;
            match self
                .read(ctx, &mut stream, scan.id, Pace::Eager, Some(limit), LONG)
                .await
            {
                Read::Limit if ctx.time().now() < deadline => {}
                read => break read,
            }
        };
        if let Read::Ended(Some(error)) = &read {
            match error.reason() {
                ErrorReason::Disconnected if !graceful => {
                    assert_always!(
                        error.execution() == Execution::Executed,
                        "a disconnect after items proves execution"
                    );
                    assert_sometimes!(true, "rpc stream ended by a producer crash");
                }
                ErrorReason::StreamFailed { code } if *code == SHUTDOWN_CODE => {
                    assert_sometimes!(true, "rpc stream failed by a graceful producer shutdown");
                }
                _ => {}
            }
        }
        self.abandon_or_judge(read, stream, scan.id, op, || {});
        self.stale = true;
    }

    /// After the faults stopped: a new stream and a unary call must succeed
    /// against the current producer.
    async fn recover(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        producer: SocketAddr,
    ) {
        for _ in 0..20 {
            if ctx.state().get::<bool>(FAULTS_DONE_KEY).unwrap_or(false) {
                break;
            }
            if pause(ctx, Duration::from_millis(500)).await.is_err() {
                return;
            }
        }
        let mut recovered = false;
        for _ in 0..10 {
            self.stale = true;
            let Some((streams, ping)) = self.discover(ctx, rpc, producer).await else {
                continue;
            };
            let probe = Probe {
                id: self.id(),
                hold_ms: 0,
            };
            let unary = ping
                .bind(rpc)
                .try_get_reply_within(&probe, Duration::from_secs(3))
                .await;
            let scan = self.scan(5, 128, End::Finish);
            let stream = self.open(
                &streams,
                rpc,
                &scan,
                Self::window(128, 2),
                StreamOp::Complete,
            );
            let normal = match stream {
                Some(stream) => {
                    self.drain(ctx, stream, scan.id, StreamOp::Complete, Pace::Eager)
                        .await
                        == Outcome::Normal
                }
                None => false,
            };
            if unary.is_ok() && normal {
                recovered = true;
                break;
            }
            if pause(ctx, Duration::from_millis(300)).await.is_err() {
                return;
            }
        }
        self.history.push(format!("recovered {recovered}"));
        assert_always!(
            recovered,
            "rpc unary and new-stream service recovered after faults stopped"
        );
    }

    /// Wait until every stream of the producer's current boot ended (at
    /// most [`RELEASE_BOUND`]): an abandoned stream must release its
    /// producer.
    async fn settle(&self, ctx: &SimContext) {
        let deadline = ctx.time().now() + RELEASE_BOUND;
        while ctx.time().now() < deadline {
            let boots = ctx.state().get::<u64>(PRODUCER_BOOTS_KEY).unwrap_or(0);
            let running = self
                .ledger
                .all_produced()
                .values()
                .any(|produced| produced.boot == boots && produced.end.is_none());
            if !running {
                return;
            }
            if pause(ctx, Duration::from_millis(50)).await.is_err() {
                return;
            }
        }
    }

    async fn drive(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        producer: SocketAddr,
    ) -> SimulationResult<()> {
        for _ in 0..self.config.operations {
            if ctx.shutdown().is_cancelled() {
                break;
            }
            let mut op = self.pick(ctx);
            if op == StreamOp::Reboot && ctx.state().get::<bool>(FAULTS_DONE_KEY).unwrap_or(false) {
                // Nobody serves reboot requests any more.
                op = StreamOp::Complete;
            }
            let Some((streams, ping)) = self.discover(ctx, rpc, producer).await else {
                continue;
            };
            self.step(op, ctx, rpc, &streams, &ping).await;
            let (low, high) = self.config.gap_ms;
            let gap = ctx.random().random_range(low..high.max(low + 1));
            if ctx.time().sleep(Duration::from_millis(gap)).await.is_err() {
                break;
            }
        }
        self.recover(ctx, rpc, producer).await;
        self.settle(ctx).await;
        // Let the producer publish its counters after the last ends were
        // written (it reports every 100 ms).
        pause(ctx, Duration::from_millis(350)).await?;
        Ok(())
    }
}

fn refs(listing: &Listing) -> Option<(Streams, ServiceRef<Ping>)> {
    Some((
        ServiceRef::<ScanItems>::from_bytes(&listing.scan).ok()?,
        ServiceRef::<Ping>::from_bytes(&listing.probe).ok()?,
    ))
}

#[async_trait]
impl Workload for StreamsWorkload {
    fn name(&self) -> &'static str {
        "rpc_streams_consumer"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        self.ledger = StreamLedger::of(ctx.state());
        let (driver, rpc) = RpcDriver::client_only(ctx.providers().clone(), streams_config())
            .map_err(|error| SimulationError::InvalidState(format!("rpc config: {error}")))?;
        let board = Board::of(ctx.state());
        let probe = rpc.probe();
        if let Some(probe) = &probe {
            board.register_probe(WORKLOAD_LABEL, probe.clone());
        }
        let Some(producer) = ctx
            .topology()
            .ips_in_group("producer")
            .into_iter()
            .next()
            .and_then(|ip| format!("{ip}:{RPC_PORT}").parse::<SocketAddr>().ok())
        else {
            return Ok(());
        };
        let result = moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            result = self.drive(ctx, &rpc, producer) => result,
            () = report_stats(&rpc, &board, WORKLOAD_LABEL, ctx) => Ok(()),
        };
        if let Some(stats) = rpc.stats() {
            board.report(WORKLOAD_LABEL, stats);
            assert_always!(
                stats.pending_calls == 0
                    && stats.streams_consuming == 0
                    && stats.stream_window_reserved == 0
                    && stats.stream_buffered_bytes == 0,
                "no stream, call or buffer outlives the workload's use of it"
            );
        }
        if let Some(probe) = &probe {
            assert_always!(
                probe.is_released(),
                "the streams workload released everything once its driver dropped"
            );
        }
        result
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let boots = ctx.state().get::<u64>(PRODUCER_BOOTS_KEY).unwrap_or(0);
        for judged in &self.judged {
            judge(&self.ledger, judged, boots);
        }
        for refused in &self.refused_probes {
            assert_always!(
                self.ledger.probes(refused.id) == 0,
                "a probe refused before admission never ran"
            );
        }
        for produced in self.ledger.all_produced().values() {
            if produced.boot == boots {
                assert_always!(
                    produced.end.is_some(),
                    "no produced stream outlives its consumer (no orphan)"
                );
            }
            if produced.waited && produced.end == Some(ProducerEnd::Finished) {
                assert_sometimes!(true, "rpc stream producer waited for credit and resumed");
            }
        }
        let board = Board::of(ctx.state());
        let workload = board
            .stats_with_prefix(WORKLOAD_LABEL)
            .first()
            .map(|(_, stats)| *stats)
            .unwrap_or_default();
        assert_sometimes!(
            workload.stream_acks_immediate > 0,
            "rpc stream item acknowledged on arrival by a waiting consumer"
        );
        assert_sometimes!(
            workload.stream_acks_popped > 0,
            "rpc stream item acknowledged when popped from the queue"
        );
        let producers = board.stats_with_prefix(PRODUCER_LABEL);
        assert_sometimes!(
            producers.iter().any(|(_, stats)| stats.ping_timeouts > 0),
            "rpc producer probed a silent caller and failed its session"
        );
        if let Some((_, producer)) = board
            .stats_with_prefix(&format!("{PRODUCER_LABEL}{boots}"))
            .first()
        {
            if producer.streams_producing != 0 {
                tracing::error!(?producer, "rpc streams producer still holds streams");
            }
            assert_always!(
                producer.streams_producing == 0,
                "a producer holds no stream once every consumer is gone"
            );
        }
        self.records
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(StreamsRecord {
                history: std::mem::take(&mut self.history),
            });
        Ok(())
    }
}

/// Judge one stream against both ledgers.
fn judge(ledger: &StreamLedger, judged: &Judged, boots: u64) {
    let consumed = ledger.consumed(judged.id);
    let produced = ledger.produced(judged.id);
    match &produced {
        None => {
            assert_always!(
                consumed.items.is_empty(),
                "stream items come only from a producer that served the stream"
            );
        }
        Some(produced) => {
            assert_always!(
                consumed.items.len() <= produced.items.len(),
                "a consumer never takes more items than its producer sent"
            );
            for (position, (seq, size, boot)) in consumed.items.iter().enumerate() {
                let position_seq = u64::try_from(position).unwrap_or(u64::MAX);
                assert_always!(
                    *seq == position_seq
                        && produced.items.get(position) == Some(size)
                        && *boot == produced.boot,
                    "every consumed item is the produced item at the same position"
                );
            }
        }
    }
    let all_items = produced
        .as_ref()
        .is_some_and(|produced| produced.items.len() == consumed.items.len());
    let end = produced.as_ref().and_then(|produced| produced.end);
    match &judged.outcome {
        Outcome::Normal => {
            assert_always!(
                end == Some(ProducerEnd::Finished) && all_items,
                "a normal end delivered every item its producer sent"
            );
        }
        Outcome::Failed(error) => match error.reason() {
            ErrorReason::StreamFailed { code } => {
                assert_always!(
                    end == Some(ProducerEnd::Failed(*code)) && all_items,
                    "a producer's failure arrives after every item it sent"
                );
            }
            ErrorReason::BrokenPromise => {
                assert_always!(
                    produced.is_none()
                        || (matches!(end, Some(ProducerEnd::Dropped | ProducerEnd::TooLarge))
                            && all_items),
                    "a broken promise follows every item of a dropped producer"
                );
            }
            ErrorReason::StreamProtocol(_) => {
                assert_always!(false, "no stream protocol violation between honest peers");
            }
            _ if error.execution() == Execution::NotAdmitted => {
                assert_always!(
                    produced.is_none(),
                    "a stream refused before admission never reached a producer"
                );
            }
            _ => {}
        },
        Outcome::NotOpened(_) => {
            assert_always!(
                produced.is_none(),
                "a stream that failed to open never reached a producer"
            );
        }
        Outcome::Abandoned => {
            if let Some(produced) = &produced {
                assert_always!(
                    produced.end.is_some() || produced.boot < boots,
                    "an abandoned stream's producer stopped (no orphan stream)"
                );
            }
        }
    }
    tracing::debug!(id = judged.id, op = ?judged.op, "rpc stream judged");
}
