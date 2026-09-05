//! A partition must not alter the bytes of a multiplexed h2 connection.
//!
//! One reused h2 connection carries several concurrent requests. A scripted
//! fault injector partitions the pair while those requests are in flight, then
//! heals it. Every request body is self-describing (one repeated tag byte), so
//! a byte stream that lost an interior range shows up as a body that is not
//! uniform, or one carrying another request's tag.
//!
//! The transport may stall the stream or break the connection — a failed
//! request is an accepted outcome here. Serving *different* bytes than the
//! peer wrote is not.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use moonpool_hyper::{ChannelConfig, H2Server, ReconnectingChannel};
use moonpool_sim::{
    FaultContext, FaultInjector, NetworkProvider, Process, SimContext, SimProviders,
    SimulationBuilder, SimulationError, SimulationResult, TaskProvider, TcpListenerTrait,
    TimeProvider, Workload, assert_always, assert_sometimes,
};

/// The single workload's IP, assigned by the builder as `10.0.0.{n}`. The
/// workload asserts this holds, so a change in the scheme fails loudly instead
/// of silently partitioning nothing.
const CLIENT_IP: &str = "10.0.0.1";

/// Body length per request: several h2 DATA frames worth of one tag byte.
const BODY_LEN: usize = 8 * 1024;

/// Concurrent requests multiplexed onto the one connection per round.
const CONCURRENCY: u8 = 4;

/// Rounds of concurrent requests the workload drives.
const ROUNDS: u8 = 12;

/// How long the pair stays reachable between partitions.
const HEALED: Duration = Duration::from_micros(700);

/// How long each partition holds.
const PARTITIONED: Duration = Duration::from_millis(3);

/// The body for a request tag: uniform, so any missing interior range shows.
fn body_for(tag: u8) -> Bytes {
    Bytes::from(vec![tag; BODY_LEN])
}

/// Whether `body` is exactly the body this tag was sent with.
fn is_body_for(tag: u8, body: &[u8]) -> bool {
    body.len() == BODY_LEN && body.iter().all(|byte| *byte == tag)
}

// ============================================================================
// Server: an echo service that validates what it receives
// ============================================================================

/// Echoes the request body back, after checking it arrived intact.
#[derive(Clone)]
struct EchoTag;

impl tower_service::Service<Request<Incoming>> for EchoTag {
    type Response = Response<Full<Bytes>>;
    type Error = Box<dyn Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Incoming>) -> Self::Future {
        Box::pin(async move {
            let body = request.into_body().collect().await?.to_bytes();
            // The tag is the body's own content: a request that arrived intact
            // is a run of one byte, of the length the client wrote.
            let tag = body.first().copied().unwrap_or_default();
            assert_always!(
                is_body_for(tag, &body),
                "h2_server_read_the_request_body_the_client_wrote"
            );
            Ok(Response::new(Full::new(body)))
        })
    }
}

struct EchoProcess;

#[async_trait]
impl Process for EchoProcess {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        let server: H2Server<SimProviders> = H2Server::new(ctx.providers());

        loop {
            let (stream, _addr) = moonpool_sim::select! {
                biased;
                accepted = listener.accept() => accepted?,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };
            let connection = server.serve_connection_with_shutdown(
                stream,
                EchoTag,
                ctx.shutdown().clone().cancelled_owned(),
            );
            ctx.task()
                .spawn_task("h2-conn", async move {
                    if let Err(error) = connection.await {
                        tracing::debug!("h2 connection ended: {error}");
                    }
                })
                .detach();
        }
    }
}

// ============================================================================
// Workload: concurrent requests over one reused connection
// ============================================================================

struct MultiplexedClient;

#[async_trait]
impl Workload for MultiplexedClient {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert_eq!(
            ctx.my_ip(),
            CLIENT_IP,
            "the fault injector partitions this workload by IP"
        );
        let server_ip = ctx
            .peer("echo")
            .ok_or_else(|| SimulationError::InvalidState("echo process not found".into()))?;

        let channel: ReconnectingChannel<SimProviders, Full<Bytes>> =
            ReconnectingChannel::new(ctx.providers(), server_ip.clone(), ChannelConfig::default());

        let mut completed = 0;
        for round in 0..ROUNDS {
            moonpool_sim::select! {
                biased;
                outcome = round_of_requests(&channel, &server_ip, round) => outcome?,
                () = ctx.shutdown().cancelled() => break,
            }
            completed += 1;
        }

        // Every partition in this run heals, so every round must get through.
        // A stream that a partition strands instead of stalling stops making
        // progress and the run is cut short by its virtual-time budget.
        assert_always!(
            completed == ROUNDS,
            "every_h2_round_completes_across_healing_partitions"
        );
        channel.close();
        Ok(())
    }
}

/// Send `CONCURRENCY` requests on the shared connection and validate every
/// response body.
async fn round_of_requests(
    channel: &ReconnectingChannel<SimProviders, Full<Bytes>>,
    server_ip: &str,
    round: u8,
) -> SimulationResult<()> {
    let uri = format!("http://{server_ip}/echo");
    let mut in_flight = Vec::new();

    for slot in 0..CONCURRENCY {
        // Tags start at 1: an empty body must not look like a valid one.
        let tag = 1 + round * CONCURRENCY + slot;
        let mut handle = channel.clone();
        if futures::future::poll_fn(|cx| {
            <ReconnectingChannel<_, _> as tower_service::Service<Request<Full<Bytes>>>>::poll_ready(
                &mut handle,
                cx,
            )
        })
        .await
        .is_err()
        {
            assert_sometimes!(true, "h2_channel_readiness_failed_under_partition");
            return Ok(());
        }

        let request = Request::builder()
            .method("POST")
            .uri(&uri)
            .body(Full::new(body_for(tag)))
            .map_err(|e| SimulationError::InvalidState(format!("request build error: {e}")))?;
        in_flight.push(async move {
            (
                tag,
                tower_service::Service::call(&mut handle, request).await,
            )
        });
    }

    for (tag, outcome) in join_all(in_flight).await {
        match outcome {
            Ok(response) => {
                let body = response
                    .into_body()
                    .collect()
                    .await
                    .map_err(|e| {
                        SimulationError::InvalidState(format!("response body error: {e}"))
                    })?
                    .to_bytes();
                // The heart of the regression: a partition may delay or kill
                // this request, but the bytes that come back are either the
                // ones this request sent, or nothing at all.
                assert_always!(
                    is_body_for(tag, &body),
                    "h2_response_carries_this_requests_own_body"
                );
                assert_sometimes!(true, "h2_request_completed_with_partitions_flapping");
            }
            Err(_) => {
                assert_sometimes!(true, "h2_request_failed_under_partition");
            }
        }
    }
    Ok(())
}

// ============================================================================
// Fault: a partition that flaps across the in-flight requests
// ============================================================================

/// Sleep on the simulated clock, surfacing a shutdown as a simulation error.
async fn sleep(ctx: &FaultContext, duration: Duration) -> SimulationResult<()> {
    ctx.time()
        .sleep(duration)
        .await
        .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))
}

struct PartitionFlap;

#[async_trait]
impl FaultInjector for PartitionFlap {
    fn name(&self) -> &'static str {
        "partition_flap"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let server_ip = ctx
            .process_ips()
            .first()
            .cloned()
            .ok_or_else(|| SimulationError::InvalidState("no process to partition".into()))?;

        while !ctx.chaos_shutdown().is_cancelled() {
            sleep(ctx, HEALED).await?;
            ctx.partition(CLIENT_IP, &server_ip)?;
            sleep(ctx, PARTITIONED).await?;
            ctx.heal_partition(CLIENT_IP, &server_ip)?;
        }
        Ok(())
    }
}

// ============================================================================
// Test
// ============================================================================

#[test]
fn a_partition_never_alters_a_multiplexed_request_body() {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(EchoProcess))
        .workload(MultiplexedClient)
        .fault_factory(|| Box::new(PartitionFlap))
        .chaos_duration(Duration::from_secs(30))
        // A healed partition must let the stalled stream continue. A stream
        // stranded by one instead drags the run out by orders of magnitude
        // (the whole workload otherwise finishes in tens of simulated ms).
        .run_time_budget(Duration::from_secs(5))
        .set_iterations(5)
        .set_debug_seeds(vec![1, 2, 3, 4, 5])
        .run();

    println!("{report}");
    assert!(
        report.seeds_failing.is_empty(),
        "failing seeds: {:?}",
        report.seeds_failing
    );
    assert!(
        report.assertion_violations.is_empty(),
        "assertion violations:\n{}",
        report
            .assertion_violations
            .iter()
            .map(|violation| format!("  - {violation}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
