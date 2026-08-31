//! A metastable failure you can see: a gRPC retry storm that outlives its cause.
//!
//! A *metastable* failure is one where a temporary trigger pushes a system into
//! a bad operating state, and that state then sustains itself after the trigger
//! is gone. The system does not recover on its own even though nothing external
//! is wrong any more and the offered workload never increased.
//!
//! ```text
//!  healthy  →  temporary slowdown  →  requests exceed the client timeout
//!           →  clients retry       →  retries add offered work
//!           →  server saturates    →  latency stays above the timeout
//!           →  more retries        →  slowdown ends
//!           →  retry traffic alone keeps the server saturated
//! ```
//!
//! This example builds the smallest system that shows it, on real gRPC over
//! moonpool's simulated network and simulated clock, and prints an ASCII
//! time-series graph so a human can look at the trajectory. There is no
//! detector here on purpose: the point is to *see* the bad state persist.
//!
//! # The experiment
//!
//! * **Server** — a `Work` RPC served by [`WORKERS`] workers. Each accepted RPC
//!   reserves the next free worker for [`SERVICE_TIME`] of simulated time;
//!   excess RPCs wait for a worker. A reservation is made when the request
//!   arrives and is **never given back**, so work the client has already
//!   abandoned keeps consuming the server. That is what closes the feedback
//!   loop: timeouts do not free capacity.
//! * **Client** — open loop. Logical request `n` starts at `n * `[`ARRIVAL_INTERVAL`]
//!   of simulated time regardless of whether earlier requests finished, so
//!   rising latency cannot secretly throttle the offered load the way a
//!   `rpc().await; sleep()` loop would. Each logical request gets
//!   [`MAX_ATTEMPTS`] attempts with a [`RPC_TIMEOUT`] deadline and no backoff.
//! * **Trigger** — between [`TRIGGER_START`] and [`TRIGGER_END`] the server's
//!   service time becomes [`TRIGGER_SERVICE_TIME`]. At [`TRIGGER_END`] it is set
//!   back to exactly [`SERVICE_TIME`]. Nothing else ever changes: same worker
//!   count, same arrival rate, same timeout, same retry budget.
//!
//! Baseline load is deliberately far below saturation, and the retry ceiling is
//! deliberately above it — which is the whole bistability:
//!
//! ```text
//! capacity          = WORKERS / SERVICE_TIME
//! offered (healthy) = 1 / ARRIVAL_INTERVAL                 <  capacity
//! offered (storm)   = MAX_ATTEMPTS / ARRIVAL_INTERVAL      >  capacity
//! ```
//!
//! # Two outcomes, one configuration
//!
//! The trigger is tuned to sit *on* the tipping point. Shorten it to 600 ms and
//! no seed ever falls into the storm; lengthen it to 700 ms and every seed
//! does. At the 650 ms checked in, roughly half of them do, and which half is
//! decided entirely by the seeded network jitter and task scheduling:
//!
//! ```text
//! seed 4  →  after the trigger: busyness 1.00, retries 150/s, goodput 0/s
//! seed 0  →  after the trigger: busyness 0.38, retries   0/s, goodput 50/s
//! ```
//!
//! Same code, same parameters, same offered load. That is the point: the bad
//! state is a *second stable operating point*, not a misconfiguration.
//!
//! # Running it
//!
//! ```text
//! cargo run --release --example metastable_grpc_retry_storm \
//!     --features hyper,prometheus -- --seed 4
//!
//! cargo run --release --example metastable_grpc_retry_storm \
//!     --features hyper,prometheus -- --search 0..64
//! ```
//!
//! `--seed` runs one seed and prints its graph (seed 4 storms, seed 0
//! recovers); `--search` runs a range and prints one summary line per seed, the
//! three windows that matter, so you can pick one to look at.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool::hyper::{ChannelConfig, H2Server, H2ServerConfig, ReconnectingChannel};
use moonpool::prelude::*;
use moonpool::prometheus::{PrometheusSource, SimCounter, SimGauge};
use moonpool::{
    Fill, Mean, MetricQuery, MetricQueryPlan, MetricSnapshot, SimProviders, SimTimeProvider,
    SimulationError, SimulationMetrics, SimulationResult, TimeError,
};
use tonic::codegen::{BoxFuture, Context as TaskContext, Poll, StdError, http};
use tonic::server::UnaryService;
use tonic::{Request, Response, Status};
use tonic_prost::ProstCodec;

// ===========================================================================
// The tuning surface — every number the experiment depends on
// ===========================================================================

/// Concurrent workers on the server. One RPC occupies one worker.
const WORKERS: u64 = 4;

/// Simulated time one RPC occupies its worker for, before and after the
/// trigger. `WORKERS / SERVICE_TIME` is the server's capacity: 160 rpc/s.
const SERVICE_TIME: Duration = Duration::from_millis(25);

/// Service time while the trigger is active. Capacity collapses to 40 rpc/s,
/// which is below the baseline arrival rate.
const TRIGGER_SERVICE_TIME: Duration = Duration::from_millis(100);

/// When the temporary slowdown starts.
const TRIGGER_START: Duration = Duration::from_secs(10);

/// When the temporary slowdown ends — completely, back to [`SERVICE_TIME`].
///
/// Deliberately set just inside the tipping point. Shortening this to 600 ms
/// leaves every seed healthy and lengthening it to 700 ms breaks every seed; at
/// 650 ms the seed decides, which is what makes the bad state visibly a second
/// stable operating point rather than a broken configuration.
const TRIGGER_END: Duration = Duration::from_millis(10_650);

/// Gap between logical request arrivals: 50 rpc/s, about a third of capacity.
const ARRIVAL_INTERVAL: Duration = Duration::from_millis(20);

/// How long the client keeps offering that same baseline load.
const OFFER_DURATION: Duration = Duration::from_secs(40);

/// Extra simulated time after the last arrival, so late requests can land.
const DRAIN: Duration = Duration::from_secs(2);

/// Per-attempt deadline. Aggressive relative to the 25 ms of service, which is
/// exactly the setting that turns a slowdown into a retry storm.
const RPC_TIMEOUT: Duration = Duration::from_millis(200);

/// Attempts per logical request, first try included. `MAX_ATTEMPTS / `
/// [`ARRIVAL_INTERVAL`] = 200 rpc/s is above the 160 rpc/s the server can do,
/// so a client population that is fully timing out offers more than the server
/// can drain — the storm sustains itself.
const MAX_ATTEMPTS: u32 = 4;

/// One column of the graph, and one point of every plotted series.
const BUCKET: Duration = Duration::from_secs(1);

/// Warm-up requests the client will send before the measured load begins.
const WARM_UP_ATTEMPTS: u32 = 40;

/// gRPC method the client calls and the server routes.
const WORK_METHOD: &str = "/moonpool.metastable.Work/Do";

// ===========================================================================
// Protobuf messages
// ===========================================================================

/// The two messages of the `Work` service.
///
/// Hand-derived rather than generated: the point of the example is queueing
/// and retries, and a `build.rs` calling `protoc` would put a toolchain
/// dependency on every consumer of this crate for two integer fields.
mod proto {
    /// One unit of work, identified so a reply can be matched to its request.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct WorkRequest {
        /// Logical request id.
        #[prost(uint64, tag = "1")]
        pub id: u64,
    }

    /// The reply, echoing the id the server just spent a worker on.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct WorkReply {
        /// The id from the request.
        #[prost(uint64, tag = "1")]
        pub id: u64,
    }
}

use proto::{WorkReply, WorkRequest};

/// Whole milliseconds of simulated time, the unit every reservation uses.
fn millis(time: &SimTimeProvider) -> u64 {
    u64::try_from(time.now().as_millis()).unwrap_or(u64::MAX)
}

/// Widen a count to `f64`. Every count here is bounded by the run length times
/// the request rate, far inside `u32`.
fn wide(value: u64) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

// ===========================================================================
// Server: a bounded pool of workers
// ===========================================================================

/// The server's finite service capacity: [`WORKERS`] workers, each busy until
/// some instant of simulated time.
///
/// `admit` hands a request the next worker to come free and moves that worker's
/// free-at instant forward by the current service time. The reservation is
/// permanent: nothing here can cancel it, so an RPC the client gives up on
/// still costs the server everything it was going to cost. Real servers behave
/// this way whenever the work is not cancellable — a query already sent to a
/// database, a job already handed to a thread pool.
struct Capacity {
    /// Per worker, the simulated instant (ms) at which it comes free.
    free_at: Mutex<Vec<u64>>,
    /// Service time in ms, moved by the trigger and moved straight back.
    service_ms: AtomicU64,
}

impl Capacity {
    fn new() -> Self {
        let service_ms = u64::try_from(SERVICE_TIME.as_millis()).unwrap_or(u64::MAX);
        Self {
            free_at: Mutex::new(vec![0; usize::try_from(WORKERS).unwrap_or(1)]),
            service_ms: AtomicU64::new(service_ms),
        }
    }

    fn set_service_time(&self, service: Duration) {
        let ms = u64::try_from(service.as_millis()).unwrap_or(u64::MAX);
        self.service_ms.store(ms, Ordering::Relaxed);
    }

    /// Reserve the next free worker for one request; returns the instant the
    /// request's work finishes.
    fn admit(&self, now_ms: u64) -> u64 {
        let mut free_at = self.lock();
        let slot = free_at
            .iter_mut()
            .min_by_key(|at| **at)
            .expect("capacity always has at least one worker");
        let done = (*slot).max(now_ms) + self.service_ms.load(Ordering::Relaxed);
        *slot = done;
        done
    }

    /// Fraction of workers busy at `now_ms`, in `0.0..=1.0`.
    fn busy_fraction(&self, now_ms: u64) -> f64 {
        let busy = self.lock().iter().filter(|at| **at > now_ms).count();
        wide(u64::try_from(busy).unwrap_or(0)) / wide(WORKERS)
    }

    /// Seconds a request arriving at `now_ms` would wait before a worker takes
    /// it: the queue, measured in the unit that matters against a timeout.
    fn backlog_seconds(&self, now_ms: u64) -> f64 {
        let earliest = self.lock().iter().copied().min().unwrap_or(now_ms);
        wide(earliest.saturating_sub(now_ms)) / 1000.0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u64>> {
        self.free_at
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }
}

/// What the server records about itself, exactly as a production service would.
///
/// `admitted` counts every RPC that took a worker; `completed` counts only the
/// ones whose handler survived to the end. The gap between them is the work the
/// client walked away from and the server did anyway, which is the mechanism
/// this example is built around.
struct ServerMetrics {
    admitted: SimCounter,
    completed: SimCounter,
    busy: SimGauge,
    backlog: SimGauge,
}

impl ServerMetrics {
    fn resolve(source: &PrometheusSource) -> SimulationResult<Self> {
        let err = |e| SimulationError::InvalidState(format!("server metrics: {e}"));
        Ok(Self {
            admitted: source
                .counter("work_admitted_total", "RPCs that reached a worker queue")
                .map_err(err)?,
            completed: source
                .counter("work_completed_total", "RPCs whose work ran to the end")
                .map_err(err)?,
            busy: source
                .gauge("work_busy_fraction", "Fraction of workers busy")
                .map_err(err)?,
            backlog: source
                .gauge("work_backlog_seconds", "Wait a new arrival would face")
                .map_err(err)?,
        })
    }
}

/// The `Work` handler: take a worker, spend the service time on it, reply.
#[derive(Clone)]
struct WorkHandler {
    capacity: Arc<Capacity>,
    metrics: Arc<ServerMetrics>,
    time: SimTimeProvider,
}

impl UnaryService<WorkRequest> for WorkHandler {
    type Response = WorkReply;
    type Future = BoxFuture<Response<WorkReply>, Status>;

    fn call(&mut self, request: Request<WorkRequest>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let arrived = millis(&this.time);
            let done = this.capacity.admit(arrived);
            this.metrics.admitted.inc();
            this.metrics.busy.set(this.capacity.busy_fraction(arrived));
            this.metrics
                .backlog
                .set(this.capacity.backlog_seconds(arrived));

            // Queueing and service, together: the reservation already covers
            // the wait for a worker, so one sleep is the whole residence time.
            this.time
                .sleep(Duration::from_millis(done.saturating_sub(arrived)))
                .await
                .map_err(|e| Status::unavailable(format!("server shutting down: {e}")))?;

            let now = millis(&this.time);
            this.metrics.completed.inc();
            this.metrics.busy.set(this.capacity.busy_fraction(now));
            Ok(Response::new(WorkReply {
                id: request.into_inner().id,
            }))
        })
    }
}

/// Routes `POST /moonpool.metastable.Work/Do` to [`WorkHandler`].
///
/// This is the shape `tonic-build` generates, written out: a tower service over
/// `http::Request`, dispatching on the path into `tonic::server::Grpc`.
#[derive(Clone)]
struct WorkService(WorkHandler);

impl<B> tonic::codegen::Service<http::Request<B>> for WorkService
where
    B: tonic::codegen::Body + Send + 'static,
    B::Error: Into<StdError> + Send,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        if request.uri().path() != WORK_METHOD {
            return Box::pin(async { Ok(Status::unimplemented("no such method").into_http()) });
        }
        let handler = self.0.clone();
        Box::pin(async move {
            let mut grpc =
                tonic::server::Grpc::new(ProstCodec::<WorkReply, WorkRequest>::default());
            Ok(grpc.unary(handler, request).await)
        })
    }
}

/// The server process: bind, serve h2, and run the one temporary trigger.
struct WorkServer;

#[async_trait]
impl Process for WorkServer {
    fn name(&self) -> &'static str {
        "work"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let source = ctx
            .metrics::<PrometheusSource>()
            .ok_or_else(|| SimulationError::InvalidState("no metrics factory".to_owned()))?;
        // A storm records far more points than the default cap holds, and a
        // truncated series would silently flatten the interesting part.
        source.set_series_capacity(Some(1_000_000));

        let capacity = Arc::new(Capacity::new());
        let service = WorkService(WorkHandler {
            capacity: capacity.clone(),
            metrics: Arc::new(ServerMetrics::resolve(&source)?),
            time: ctx.time().clone(),
        });
        Self::spawn_trigger(ctx, capacity);

        let listener = ctx.network().bind(ctx.my_ip()).await?;
        let server = H2Server::new(ctx.providers()).with_config(H2ServerConfig::default());
        loop {
            moonpool::select! {
                accepted = listener.accept() => {
                    let (stream, _addr) = accepted?;
                    let connection = server.serve_connection_with_shutdown(
                        stream,
                        service.clone(),
                        ctx.shutdown().clone().cancelled_owned(),
                    );
                    ctx.task()
                        .spawn_task("work-conn", async move { drop(connection.await) })
                        .detach();
                }
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}

impl WorkServer {
    /// The one and only disturbance: service time goes up for a while, then
    /// back to exactly what it was. Nothing else in the run ever changes.
    fn spawn_trigger(ctx: &SimContext, capacity: Arc<Capacity>) {
        let time = ctx.time().clone();
        ctx.task()
            .spawn_task("slowdown-trigger", async move {
                if time.sleep(TRIGGER_START).await.is_err() {
                    return;
                }
                capacity.set_service_time(TRIGGER_SERVICE_TIME);
                if time
                    .sleep(TRIGGER_END.saturating_sub(TRIGGER_START))
                    .await
                    .is_err()
                {
                    return;
                }
                capacity.set_service_time(SERVICE_TIME);
            })
            .detach();
    }
}

// ===========================================================================
// Client: open-loop arrivals with an aggressive timeout and retries
// ===========================================================================

/// What the client records: offered load, attempts, and what came back.
struct ClientMetrics {
    arrivals: SimCounter,
    attempts: SimCounter,
    retries: SimCounter,
    timeouts: SimCounter,
    goodput: SimCounter,
    in_flight: SimGauge,
}

impl ClientMetrics {
    fn resolve(source: &PrometheusSource) -> SimulationResult<Self> {
        let err = |e| SimulationError::InvalidState(format!("client metrics: {e}"));
        Ok(Self {
            arrivals: source
                .counter("client_arrivals_total", "Logical requests offered")
                .map_err(err)?,
            attempts: source
                .counter("client_attempts_total", "RPC attempts sent")
                .map_err(err)?,
            retries: source
                .counter("client_retries_total", "Attempts that were retries")
                .map_err(err)?,
            timeouts: source
                .counter("client_timeouts_total", "Attempts that hit the deadline")
                .map_err(err)?,
            goodput: source
                .counter("client_goodput_total", "Logical requests that succeeded")
                .map_err(err)?,
            in_flight: source
                .gauge("client_in_flight", "Logical requests not yet resolved")
                .map_err(err)?,
        })
    }
}

/// The channel every attempt shares, the role `tonic::transport::Channel`
/// plays in production.
type WorkChannel = ReconnectingChannel<SimProviders, tonic::body::Body>;

/// The open-loop load generator.
struct StormWorkload;

#[async_trait]
impl Workload for StormWorkload {
    fn name(&self) -> &'static str {
        "open-loop-client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let source = ctx
            .metrics::<PrometheusSource>()
            .ok_or_else(|| SimulationError::InvalidState("no metrics factory".to_owned()))?;
        source.set_series_capacity(Some(1_000_000));
        let metrics = Arc::new(ClientMetrics::resolve(&source)?);

        let server_ip = ctx
            .peer("work")
            .ok_or_else(|| SimulationError::InvalidState("work process not found".to_owned()))?;
        let channel: WorkChannel = ReconnectingChannel::new(
            ctx.providers(),
            server_ip.clone(),
            ChannelConfig {
                connection_timeout: Duration::from_secs(2),
                ..ChannelConfig::default()
            },
        );
        let origin = http::Uri::try_from(format!("http://{server_ip}"))
            .map_err(|e| SimulationError::InvalidState(format!("bad origin: {e}")))?;

        // Wait until the stack is actually up before the arrival clock starts.
        // Otherwise the first second of "offered load" piles up behind the TCP
        // connect and the h2 handshake and releases as one burst, which can
        // ignite the storm before the experiment has even begun.
        Self::warm_up(ctx, &channel, &origin).await?;
        Self::offer(ctx, &channel, &origin, &metrics).await?;

        // Let whatever is still in flight resolve before the run ends.
        ctx.time().sleep(DRAIN).await.ok();
        channel.close();
        Ok(())
    }
}

impl StormWorkload {
    /// Send unmetered requests until one succeeds, so the measured run starts
    /// from a connected channel and an idle server.
    async fn warm_up(
        ctx: &SimContext,
        channel: &WorkChannel,
        origin: &http::Uri,
    ) -> SimulationResult<()> {
        let time = ctx.time().clone();
        for _ in 0..WARM_UP_ATTEMPTS {
            if time
                .timeout(RPC_TIMEOUT, Self::attempt(channel, origin, 0))
                .await
                .is_ok_and(|outcome| outcome.is_ok())
            {
                // Let the warm-up request's worker time expire before the
                // arrival clock starts.
                time.sleep(SERVICE_TIME).await.ok();
                return Ok(());
            }
            time.sleep(RPC_TIMEOUT).await.ok();
        }
        Err(SimulationError::InvalidState(
            "server never answered a warm-up request".to_owned(),
        ))
    }

    /// Open-loop arrivals: request `n` starts at `n * ARRIVAL_INTERVAL` of
    /// simulated time, measured from a fixed origin.
    ///
    /// Scheduling against absolute time rather than sleeping *between*
    /// requests is the whole point. A closed loop
    /// (`rpc().await; sleep(interval).await`) silently reduces its own offered
    /// rate as latency rises, which hides overload — precisely the effect
    /// under study.
    async fn offer(
        ctx: &SimContext,
        channel: &WorkChannel,
        origin: &http::Uri,
        metrics: &Arc<ClientMetrics>,
    ) -> SimulationResult<()> {
        let time = ctx.time().clone();
        let start = time.now();
        let count = OFFER_DURATION.as_nanos() / ARRIVAL_INTERVAL.as_nanos();
        for n in 0..u64::try_from(count).unwrap_or(0) {
            let due = start + ARRIVAL_INTERVAL * u32::try_from(n).unwrap_or(u32::MAX);
            let now = time.now();
            if due > now && time.sleep(due.saturating_sub(now)).await.is_err() {
                // Shutdown during the offer window: stop generating load.
                break;
            }

            metrics.arrivals.inc();
            metrics.in_flight.inc();
            let request = Self::logical_request(
                channel.clone(),
                origin.clone(),
                time.clone(),
                metrics.clone(),
                n,
            );
            // Detached: an arrival must not wait on the previous one.
            ctx.task().spawn_task("logical-request", request).detach();
        }
        Ok(())
    }

    /// One logical request: attempt, and on a timeout attempt again at once.
    ///
    /// No backoff, a small fixed budget — the ordinary aggressive client
    /// policy that turns a latency excursion into a load multiplier.
    async fn logical_request(
        channel: WorkChannel,
        origin: http::Uri,
        time: SimTimeProvider,
        metrics: Arc<ClientMetrics>,
        id: u64,
    ) {
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                metrics.retries.inc();
            }
            metrics.attempts.inc();

            // Dropping the RPC future on the deadline resets the h2 stream, but
            // the worker the server already reserved stays reserved.
            match time
                .timeout(RPC_TIMEOUT, Self::attempt(&channel, &origin, id))
                .await
            {
                Ok(Ok(())) => {
                    metrics.goodput.inc();
                    break;
                }
                Err(TimeError::Elapsed) => metrics.timeouts.inc(),
                Err(TimeError::Shutdown) => break,
                Ok(Err(_status)) => {}
            }
        }
        metrics.in_flight.dec();
    }

    /// One gRPC attempt over the shared channel.
    async fn attempt(channel: &WorkChannel, origin: &http::Uri, id: u64) -> Result<(), Status> {
        // A fresh clone per attempt: tower readiness is reserved per handle, so
        // concurrent attempts must not share one.
        let mut grpc = tonic::client::Grpc::with_origin(channel.clone(), origin.clone());
        grpc.ready()
            .await
            .map_err(|e| Status::unavailable(format!("channel not ready: {e}")))?;
        grpc.unary(
            Request::new(WorkRequest { id }),
            http::uri::PathAndQuery::from_static(WORK_METHOD),
            ProstCodec::<WorkRequest, WorkReply>::default(),
        )
        .await
        .map(|_response| ())
    }
}

// ===========================================================================
// Running one seed
// ===========================================================================

/// Run the whole experiment once and hand back what that seed recorded.
fn run_seed(seed: u64) -> Result<SimulationMetrics, String> {
    let report = SimulationBuilder::new()
        // One registry per simulated node: server and client counters must not
        // be merged into one number.
        .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
        .processes(1, || Box::new(WorkServer))
        .workload(StormWorkload)
        .set_debug_seeds(vec![seed])
        .set_iterations(1)
        .run();

    match report.individual_metrics.into_iter().next() {
        Some(Ok(metrics)) => Ok(metrics),
        Some(Err(e)) => Err(format!("seed {seed} failed: {e}")),
        None => Err(format!("seed {seed} produced no metrics")),
    }
}

/// The bucketed series the graph draws, in the order they are printed.
///
/// Every one is a query over the series the application already recorded — the
/// same Prometheus counters and gauges a production deployment would scrape.
fn plans() -> Vec<(&'static str, MetricQueryPlan)> {
    let rate = |metric: &str, name: &str| {
        MetricQuery::select(metric)
            .rate()
            .bucketize(BUCKET, Mean)
            // A second with no events really is zero per second.
            .fill(Fill::Value(0.0))
            .reduce(Mean)
            .named(name.to_owned())
    };
    let level = |metric: &str, name: &str| {
        MetricQuery::select(metric)
            .bucketize(BUCKET, Mean)
            // A gauge nobody touched still holds its last value.
            .fill(Fill::Previous)
            .reduce(Mean)
            .named(name.to_owned())
    };
    vec![
        (
            "server busyness   busy workers / 4",
            level("work_busy_fraction", "busyness"),
        ),
        (
            "server queue      seconds a new request waits",
            level("work_backlog_seconds", "backlog"),
        ),
        (
            "offered load      logical arrivals/s  (never changes)",
            rate("client_arrivals_total", "arrivals"),
        ),
        (
            "rpc attempts      first tries + retries/s",
            rate("client_attempts_total", "attempts"),
        ),
        (
            "retries           retry attempts/s",
            rate("client_retries_total", "retries"),
        ),
        (
            "timeouts          attempts that hit the deadline/s",
            rate("client_timeouts_total", "timeouts"),
        ),
        (
            "goodput           logical requests completed/s",
            rate("client_goodput_total", "goodput"),
        ),
        (
            "client in-flight  logical requests unresolved",
            level("client_in_flight", "in_flight"),
        ),
    ]
}

/// One row of the graph: a title and one value per time bucket.
struct Panel {
    /// Header printed above the bars.
    title: String,
    /// Short name, used by the phase table.
    name: String,
    /// One value per time bucket; `None` where the query had nothing to say.
    values: Vec<Option<f64>>,
}

/// Turn one run's recorded series into the panels the graph draws.
fn panels(metrics: &SimulationMetrics, seed: u64) -> (Vec<Panel>, usize) {
    let end_ms = u64::try_from(metrics.simulated_time.as_millis()).unwrap_or(u64::MAX);
    let bucket_ms = u64::try_from(BUCKET.as_millis()).unwrap_or(1);
    let columns = usize::try_from(end_ms / bucket_ms).unwrap_or(0);
    let snapshot = MetricSnapshot::from_run(&metrics.app_metrics, &metrics.app_series, end_ms);

    let panels = plans()
        .into_iter()
        .map(|(title, plan)| {
            let mut values = vec![None; columns];
            for row in plan.evaluate(&snapshot, 0, seed) {
                let index = usize::try_from(row.bucket_start_ms / bucket_ms).unwrap_or(usize::MAX);
                if let Some(slot) = values.get_mut(index) {
                    *slot = Some(row.value);
                }
            }
            Panel {
                title: title.to_owned(),
                name: plan.name().to_owned(),
                values,
            }
        })
        .collect();
    (panels, columns)
}

// ===========================================================================
// The ASCII graph
// ===========================================================================

/// Rows of blocks per panel. Five is enough to read a shape and keeps the
/// whole graph on one screen.
const PANEL_HEIGHT: usize = 5;

/// Width of the y-axis label gutter, `"  1.00 |"`.
const GUTTER: usize = 8;

/// Columns at the end of the run that count as "long after the trigger".
///
/// The window ends where the client stops offering load, so the comparison is
/// between two stretches under exactly the same offered rate.
const TAIL_COLUMNS: usize = 10;

/// `(start, end)` columns of the tail window.
fn tail_window() -> (usize, usize) {
    let end = column_of(OFFER_DURATION);
    (end.saturating_sub(TAIL_COLUMNS), end)
}

/// The column a simulated instant falls in.
fn column_of(at: Duration) -> usize {
    usize::try_from(at.as_millis() / BUCKET.as_millis()).unwrap_or(0)
}

/// Format a y-axis tick, adapting to the magnitude of the series.
fn tick(value: f64, top: f64) -> String {
    if top >= 10.0 {
        format!("{value:>6.0}")
    } else {
        format!("{value:>6.2}")
    }
}

/// Draw one panel as `PANEL_HEIGHT` bar rows plus an axis.
///
/// Each panel is scaled to its own peak, printed in the header so the shape and
/// the magnitude can both be read off.
fn draw(panel: &Panel, columns: usize) -> String {
    let top = panel
        .values
        .iter()
        .filter_map(|v| *v)
        .fold(0.0_f64, f64::max)
        .max(f64::MIN_POSITIVE);
    let height = wide(u64::try_from(PANEL_HEIGHT).unwrap_or(1));

    let mut out = format!("{:<54}peak {}\n", panel.title, tick(top, top).trim_start());
    for row in (1..=PANEL_HEIGHT).rev() {
        let row_index = wide(u64::try_from(row).unwrap_or(0));
        // A column fills this row when its value clears the row below it, so
        // any non-zero value shows up on the bottom row.
        let floor = top * (row_index - 1.0) / height;
        let bars: String = panel
            .values
            .iter()
            .map(|value| match value {
                Some(v) if *v > floor => '#',
                _ => ' ',
            })
            .collect();
        let label = tick(top * row_index / height, top);
        out.push_str(format!("{label} |{bars}").trim_end());
        out.push('\n');
    }
    let _ = writeln!(out, "{} +{}", tick(0.0, top), "-".repeat(columns));
    out
}

/// The columns the trigger window touches, as a half-open range.
///
/// Rounded outwards, so a trigger shorter than one bucket still occupies a
/// column instead of vanishing between two.
fn trigger_columns() -> (usize, usize) {
    let on = column_of(TRIGGER_START);
    let off = column_of(TRIGGER_END).max(on) + usize::from(!is_on_bucket(TRIGGER_END));
    (on, off)
}

/// Whether an instant falls exactly on a bucket boundary.
fn is_on_bucket(at: Duration) -> bool {
    at.as_millis().is_multiple_of(BUCKET.as_millis())
}

/// The band marking the trigger, aligned to the same columns as every panel.
fn trigger_band(columns: usize) -> String {
    let (on, off) = trigger_columns();
    let band: String = (0..columns)
        .map(|c| if c >= on && c < off { 'T' } else { '.' })
        .collect();
    // "trigger " is exactly GUTTER wide, so the band lines up with the bars.
    format!("trigger {band}")
}

/// A ruler with a tick every ten columns.
fn time_ruler(columns: usize) -> String {
    let ticks: String = (0..columns)
        .map(|c| if c % 10 == 0 { '|' } else { ' ' })
        .collect();
    let mut labels = String::new();
    for c in (0..columns).step_by(10) {
        let seconds = u64::try_from(c).unwrap_or(0) * BUCKET.as_secs();
        let label = format!("{seconds}s");
        labels.push_str(&label);
        labels.push_str(&" ".repeat(10_usize.saturating_sub(label.len())));
    }
    format!(
        "{}{}\n{}{}",
        " ".repeat(GUTTER),
        ticks.trim_end(),
        " ".repeat(GUTTER),
        labels.trim_end()
    )
}

/// The whole graph for one seed.
fn render(seed: u64, panels: &[Panel], columns: usize) -> String {
    let capacity =
        wide(WORKERS) * 1000.0 / wide(u64::try_from(SERVICE_TIME.as_millis()).unwrap_or(1));
    let offered = 1000.0 / wide(u64::try_from(ARRIVAL_INTERVAL.as_millis()).unwrap_or(1));
    let mut out = format!(
        "\nMetastable gRPC retry storm — seed {seed}\n\
         \x20 server   {WORKERS} workers x {} ms  => capacity {capacity:.0} rpc/s\n\
         \x20 client   open loop {offered:.0} req/s, timeout {} ms, {MAX_ATTEMPTS} attempts, no backoff\n\
         \x20 trigger  service time {} ms -> {} ms from t={:.3}s to t={:.3}s, then exactly {} ms again\n\
         \x20 one column = {} s of simulated time;  T marks the trigger window\n\n",
        SERVICE_TIME.as_millis(),
        RPC_TIMEOUT.as_millis(),
        SERVICE_TIME.as_millis(),
        TRIGGER_SERVICE_TIME.as_millis(),
        TRIGGER_START.as_secs_f64(),
        TRIGGER_END.as_secs_f64(),
        SERVICE_TIME.as_millis(),
        BUCKET.as_secs(),
    );
    let _ = writeln!(out, "{}", trigger_band(columns));
    for panel in panels {
        out.push_str(&draw(panel, columns));
    }
    let _ = writeln!(out, "{}", trigger_band(columns));
    let _ = writeln!(out, "{}", time_ruler(columns));
    out.push_str(&phase_table(panels));
    out
}

/// The same series as three numbers each: healthy, triggered, and long after
/// the trigger is gone.
///
/// The graph shows the shape; this shows that the "after" column really is the
/// bad state and not a rendering artifact.
fn phase_table(panels: &[Panel]) -> String {
    let (on, off) = trigger_columns();
    let (tail, tail_end) = tail_window();
    let mut out = format!(
        "\n{:<12}  {:>16}  {:>16}  {:>16}\n",
        "series",
        format!("before 2-{on}s"),
        format!("during {on}-{off}s"),
        format!("after {tail}-{tail_end}s"),
    );
    for panel in panels {
        let _ = writeln!(
            out,
            "{:<12}  {:>16.2}  {:>16.2}  {:>16.2}",
            panel.name,
            window_mean(panel, 2, on),
            window_mean(panel, on, off),
            window_mean(panel, tail, tail_end),
        );
    }
    out
}

// ===========================================================================
// Seed search
// ===========================================================================

/// Mean of a panel's values over `[from, to)` columns, ignoring empty ones.
fn window_mean(panel: &Panel, from: usize, to: usize) -> f64 {
    let values: Vec<f64> = panel
        .values
        .get(from..to.min(panel.values.len()))
        .unwrap_or(&[])
        .iter()
        .filter_map(|v| *v)
        .collect();
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / wide(u64::try_from(values.len()).unwrap_or(1))
}

/// Run a range of seeds and print one line each, so a human can pick one.
///
/// Deliberately just arithmetic over the windows that matter, and no verdict:
/// the numbers are printed and the reader decides which seed to look at. A seed
/// whose `post gp/s` is 0 while `post busy` is 1.00 stayed in the storm; one
/// that reads 50 and 0.38 went back to the healthy operating point.
fn search(from: u64, to: u64) {
    println!(
        "{:>6}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
        "seed", "pre gp/s", "post gp/s", "post busy", "post rty/s", "post queue"
    );
    for seed in from..to {
        let Ok(metrics) = run_seed(seed) else {
            println!("{seed:>6}  (failed)");
            continue;
        };
        let (panels, _columns) = panels(&metrics, seed);
        let pre = column_of(TRIGGER_START);
        let (tail, tail_end) = tail_window();
        let series = |name: &str, from, to| {
            panels
                .iter()
                .find(|p| p.name == name)
                .map_or(0.0, |p| window_mean(p, from, to))
        };
        println!(
            "{seed:>6}  {:>10.1}  {:>10.1}  {:>10.2}  {:>10.1}  {:>10.2}",
            series("goodput", 2, pre),
            series("goodput", tail, tail_end),
            series("busyness", tail, tail_end),
            series("retries", tail, tail_end),
            series("backlog", tail, tail_end),
        );
    }
}

// ===========================================================================
// Entry point
// ===========================================================================

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some("--search") = args.first().map(String::as_str) {
        let range = args.get(1).map_or("0..32", String::as_str);
        let (from, to) = range.split_once("..").unwrap_or(("0", "32"));
        search(from.parse().unwrap_or(0), to.parse().unwrap_or(32));
        return;
    }

    let seed = args
        .iter()
        .position(|a| a == "--seed")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEMO_SEED);
    match graph(seed) {
        Ok(graph) => println!("{graph}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// Run one seed and render its graph, keeping the panels the graph was drawn
/// from so a caller can read numbers off them too.
///
/// # Errors
///
/// Returns the seed's failure message when the simulation did not complete.
fn graph_with_panels(seed: u64) -> Result<(Vec<Panel>, String), String> {
    let metrics = run_seed(seed)?;
    let (panels, columns) = panels(&metrics, seed);
    let graph = render(seed, &panels, columns);
    Ok((panels, graph))
}

/// Run one seed and render its graph.
///
/// # Errors
///
/// Returns the seed's failure message when the simulation did not complete.
fn graph(seed: u64) -> Result<String, String> {
    graph_with_panels(seed).map(|(_panels, graph)| graph)
}

/// The seed the demo prints by default: one that stays in the storm.
const DEMO_SEED: u64 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole claim of this example, on the seed it ships with.
    ///
    /// Two runs of the same seed must render the same graph — a metastable run
    /// is only worth reporting if it replays — and that graph must show a run
    /// that starts healthy, is disturbed for 650 ms, and is still in the bad
    /// state half a minute later under exactly the same offered load.
    ///
    /// A regression test on one recorded seed, not a detector: it reads the
    /// same three windows a human reads off the graph.
    #[test]
    fn the_demo_seed_replays_a_storm_that_outlives_its_trigger() {
        let (panels, first) = graph_with_panels(DEMO_SEED).expect("demo seed runs");
        let (_, second) = graph_with_panels(DEMO_SEED).expect("demo seed runs again");
        assert_eq!(first, second, "the same seed must replay identically");

        let (on, _off) = trigger_columns();
        let (tail, tail_end) = tail_window();
        let mean = |name: &str, from, to| {
            panels
                .iter()
                .find(|p| p.name == name)
                .map_or(0.0, |p| window_mean(p, from, to))
        };

        assert!(
            mean("goodput", 2, on) > 45.0,
            "the run must start healthy, not already broken"
        );
        assert!(
            mean("busyness", 2, on) < 0.6,
            "the healthy phase must be well below saturation"
        );
        assert!(
            mean("arrivals", tail, tail_end) > 45.0,
            "the offered load must be unchanged in the tail window"
        );
        assert!(
            mean("busyness", tail, tail_end) > 0.95,
            "the server must still be saturated long after the trigger"
        );
        assert!(
            mean("goodput", tail, tail_end) < 5.0,
            "goodput must still be on the floor long after the trigger"
        );
        assert!(
            mean("retries", tail, tail_end) > 50.0,
            "retries must still be sustaining the load"
        );
    }
}
