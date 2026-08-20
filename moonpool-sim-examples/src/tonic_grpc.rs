//! tonic gRPC service simulation example.
//!
//! Demonstrates how to test a real tonic-based gRPC service — protobuf
//! definitions compiled by `protoc` via `tonic-prost-build` — inside
//! moonpool-sim's deterministic simulation with chaos injection.
//!
//! The example deliberately avoids `tonic::transport`, which is hard-coupled
//! to the tokio runtime (`tokio::spawn` per connection, hard-coded
//! `TokioExecutor`/`TokioTimer`) and therefore cannot run inside a
//! simulation. Instead it uses tonic's runtime-free core plus generated
//! stubs, and drives hyper's HTTP/2 connection API directly:
//!
//! - **IO**: `SimTcpStream` (futures-io) → `Compat` (tokio-io) → `TokioIo`
//!   (hyper-io), the same bridge as the axum example.
//! - **Spawning**: [`ProviderExecutor`] implements `hyper::rt::Executor` on
//!   top of moonpool's `TaskProvider`, so hyper's h2 internals spawn onto the
//!   deterministic sim executor.
//! - **gRPC**: the generated `EchoServer`/`EchoClient` from
//!   `proto/echo.proto` — the exact same codegen output production uses.
//!
//! Unlike hyper's HTTP/1 connection state machine (which is `!Send` and must
//! be driven inline — see `axum_web.rs`), h2 connections are `Send`, so both
//! server and client connection futures are spawned as ordinary sim tasks.
//!
//! What the workload exercises per round, all over one multiplexed h2
//! connection:
//!
//! - **Concurrent unary RPCs** (cloned clients, joined futures) with
//!   metadata round-tripping and per-call deadlines via the time provider
//! - **Server streaming** with in-order delivery checks and buggified
//!   mid-stream aborts
//! - **Unknown-service probe** against the unmounted `Shout` service
//!   (expects `UNIMPLEMENTED`)
//!
//! The sim binary adds Attrition chaos (server crash/reboot) on top of the
//! default network chaos, so rounds also see dead servers and reconnects.
//!
//! # Architecture
//!
//! - **[`proto`]**: protoc/prost-generated messages and stubs
//! - **[`ProviderExecutor`]**: `hyper::rt::Executor` over any `TaskProvider`
//! - **[`EchoProcess`]**: accepts TCP, serves the generated `EchoServer`
//!   over hyper h2
//! - **[`EchoWorkload`]**: drives the RPC mix, validates responses under
//!   chaos

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tonic::metadata::MetadataValue;
use tonic::{Code, Request, Response, Status};

use moonpool_sim::{
    Detach, NetworkProvider, Process, SimContext, SimTimeProvider, SimulationError,
    SimulationResult, TaskProvider, TcpListenerTrait, TimeError, TimeProvider, Workload,
};

/// Protobuf messages and gRPC stubs generated from `proto/echo.proto` by
/// `tonic-prost-build` (requires `protoc`).
pub mod proto {
    // Generated code is exempt from the crate's lint bar: it cannot be
    // hand-fixed, and regenerating on every toolchain bump is upstream's
    // concern (prost/tonic), not this crate's.
    #![allow(missing_docs, clippy::pedantic)]
    tonic::include_proto!("moonpool.sim");
}

use proto::echo_client::EchoClient;
use proto::echo_server::{Echo, EchoServer};
use proto::shout_client::ShoutClient;
use proto::{EchoRequest, EchoResponse, EchoStreamRequest};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Metadata key the client stamps on each request; the server must echo it
/// back on the response.
const ROUND_METADATA_KEY: &str = "x-moonpool-round";

/// Per-RPC deadline. Generous against ordinary chaos delays, short enough to
/// cut through clogged connections and dead servers.
const RPC_DEADLINE: Duration = Duration::from_secs(5);

/// Items requested per streaming call.
const STREAM_COUNT: u64 = 4;

// ============================================================================
// hyper executor shim — the moonpool/hyper integration point
// ============================================================================

/// A `hyper::rt::Executor` backed by a moonpool [`TaskProvider`].
///
/// hyper's HTTP/2 implementation requires an executor to spawn internal
/// tasks (per-request service futures on the server, stream bookkeeping on
/// the client). In production that is `hyper_util::rt::TokioExecutor`; inside
/// the simulation this shim routes those spawns onto the deterministic sim
/// executor instead.
#[derive(Clone, Debug)]
pub struct ProviderExecutor<T> {
    tasks: T,
}

impl<T: TaskProvider> ProviderExecutor<T> {
    /// Create an executor that spawns via the given task provider.
    pub fn new(tasks: T) -> Self {
        Self { tasks }
    }
}

impl<T, Fut> hyper::rt::Executor<Fut> for ProviderExecutor<T>
where
    T: TaskProvider,
    Fut: Future + Send + 'static,
{
    fn execute(&self, fut: Fut) {
        // Fire-and-forget, matching hyper's TokioExecutor: hyper manages the
        // lifetime of its internal futures itself.
        self.tasks
            .spawn_task("hyper-h2", async move {
                let _ = fut.await;
            })
            .detach();
    }
}

// ============================================================================
// Server side — the generated Echo trait, implemented + served
// ============================================================================

struct EchoService {
    served: AtomicU64,
}

#[tonic::async_trait]
impl Echo for EchoService {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        // Fault injection at the application layer: a real deployment sheds
        // load or hits internal errors; the client must handle UNAVAILABLE.
        if moonpool_sim::buggify!() {
            return Err(Status::unavailable("buggified echo failure"));
        }

        let round_meta = request.metadata().get(ROUND_METADATA_KEY).cloned();
        let msg = request.into_inner();
        let served = self.served.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(seq = msg.seq, served, "echo_served");

        let mut response = Response::new(EchoResponse {
            text: msg.text,
            seq: msg.seq,
        });
        // Echo the client's round marker back so the workload can verify
        // metadata survives the full request/response path.
        if let Some(round) = round_meta {
            response.metadata_mut().insert(ROUND_METADATA_KEY, round);
        }
        Ok(response)
    }

    type EchoStreamStream = Pin<Box<dyn Stream<Item = Result<EchoResponse, Status>> + Send>>;

    async fn echo_stream(
        &self,
        request: Request<EchoStreamRequest>,
    ) -> Result<Response<Self::EchoStreamStream>, Status> {
        if moonpool_sim::buggify!() {
            return Err(Status::unavailable("buggified stream refusal"));
        }

        let msg = request.into_inner();
        tracing::info!(count = msg.count, "echo_stream_started");

        // Precompute the items so per-item fault decisions are made inside
        // the request task (deterministic per seed): the stream can abort
        // partway, but items delivered before the abort stay in order.
        let mut items = Vec::new();
        for seq in 0..msg.count {
            if moonpool_sim::buggify_with_prob!(0.05) {
                items.push(Err(Status::aborted("buggified stream abort")));
                break;
            }
            items.push(Ok(EchoResponse {
                text: msg.text.clone(),
                seq,
            }));
        }
        Ok(Response::new(Box::pin(futures::stream::iter(items))))
    }
}

/// A gRPC echo server running as a moonpool Process.
pub struct EchoProcess;

#[async_trait]
impl Process for EchoProcess {
    fn name(&self) -> &'static str {
        "grpc"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // The generated EchoServer is a plain (cloneable) tower Service; the
        // Shout service from the same .proto is intentionally NOT mounted, so
        // calls to it exercise the UNIMPLEMENTED fallback.
        let echo = EchoServer::new(EchoService {
            served: AtomicU64::new(0),
        });
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        let executor = ProviderExecutor::new(ctx.task().clone());
        tracing::info!("grpc server listening");

        loop {
            moonpool_sim::select! {
                accept = listener.accept() => {
                    let (stream, addr) = accept?;
                    tracing::info!(%addr, "accepted connection");

                    let io = TokioIo::new(stream.compat());
                    let service = TowerToHyperService::new(echo.clone());
                    let conn = hyper::server::conn::http2::Builder::new(executor.clone())
                        .serve_connection(io, service);

                    // Unlike hyper's HTTP/1 connection (which is !Send and must
                    // be driven inline), the h2 connection future is Send, so it
                    // runs as an ordinary spawned sim task.
                    ctx.task()
                        .spawn_task("grpc-server-conn", async move {
                            if let Err(e) = conn.await {
                                tracing::warn!("h2 connection error (expected under chaos): {e}");
                            }
                        })
                        .detach();
                }
                () = ctx.shutdown().cancelled() => {
                    tracing::info!("grpc server shutting down");
                    return Ok(());
                }
            }
        }
    }
}

// ============================================================================
// Client side — channel service + Workload
// ============================================================================

/// Adapts hyper's h2 `SendRequest` into the tower `Service` shape that the
/// generated clients consume — the role `tonic::transport::Channel` plays in
/// production. `Clone` shares the underlying h2 connection, so multiple
/// clients multiplex over one simulated TCP stream.
#[derive(Clone)]
struct H2Channel {
    inner: hyper::client::conn::http2::SendRequest<tonic::body::Body>,
}

impl tower_service::Service<http::Request<tonic::body::Body>> for H2Channel {
    type Response = http::Response<hyper::body::Incoming>;
    type Error = BoxError;
    type Future = BoxFut<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<tonic::body::Body>) -> Self::Future {
        let fut = self.inner.send_request(req);
        Box::pin(async move { fut.await.map_err(Into::into) })
    }
}

/// Test driver that sends concurrent unary, streaming, and unknown-service
/// RPCs and validates responses.
pub struct EchoWorkload;

#[async_trait]
impl Workload for EchoWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server_ip = ctx
            .peer("grpc")
            .ok_or_else(|| SimulationError::InvalidState("grpc process not found".into()))?;
        tracing::info!(%server_ip, "workload starting");

        for round in 0..5u32 {
            tracing::info!(round, "starting round");
            let result = moonpool_sim::select! {
                biased;
                result = Self::run_round(ctx, &server_ip, round) => result,
                () = ctx.shutdown().cancelled() => {
                    tracing::info!(round, "shutdown during round, exiting");
                    break;
                }
            };
            match result {
                Ok(()) => {
                    tracing::info!(round, "round completed successfully");
                }
                Err(e) => {
                    // Under chaos (connection resets, clogs, bit flips, server
                    // reboots), RPCs fail. That's expected — we're testing
                    // resilience.
                    moonpool_sim::assert_sometimes!(true, "grpc_round_failed");
                    tracing::warn!(round, "round failed (expected under chaos): {e}");
                }
            }

            // Spread rounds across sim time so Attrition chaos (server
            // crash/reboot windows) overlaps the workload instead of firing
            // after it already finished.
            let pause = moonpool_sim::select! {
                biased;
                result = ctx.time().sleep(Duration::from_secs(2)) => result,
                () = ctx.shutdown().cancelled() => break,
            };
            if pause.is_err() {
                break;
            }
        }

        tracing::info!("workload finished all rounds");
        Ok(())
    }
}

impl EchoWorkload {
    async fn run_round(ctx: &SimContext, server_ip: &str, round: u32) -> SimulationResult<()> {
        let time = ctx.time().clone();

        tracing::info!(round, "connecting to server");
        // Deadline on connect: with Attrition chaos the server may simply be
        // dead, and a connect to a dead process hangs rather than erroring.
        let Ok(connected) = time
            .timeout(RPC_DEADLINE, ctx.network().connect(server_ip))
            .await
        else {
            moonpool_sim::assert_sometimes!(true, "grpc_connect_timed_out");
            return Err(SimulationError::InvalidState("connect timed out".into()));
        };
        let stream = connected?;

        let io = TokioIo::new(stream.compat());
        let executor = ProviderExecutor::new(ctx.task().clone());
        let (send_request, conn) = hyper::client::conn::http2::Builder::new(executor)
            .handshake(io)
            .await
            .map_err(|e| SimulationError::InvalidState(format!("h2 handshake: {e}")))?;
        tracing::info!(round, "h2 handshake complete");

        // SendRequest only makes progress while the connection future is
        // polled. The h2 client connection is Send, so spawn it as the
        // connection driver (no inline select! dance needed as with HTTP/1).
        ctx.task()
            .spawn_task("grpc-client-conn", async move {
                if let Err(e) = conn.await {
                    tracing::warn!("client conn error (expected under chaos): {e}");
                }
            })
            .detach();

        let channel = H2Channel {
            inner: send_request,
        };
        let origin = http::Uri::try_from(format!("http://{server_ip}"))
            .map_err(|e| SimulationError::InvalidState(format!("bad origin: {e}")))?;
        let echo_client = EchoClient::with_origin(channel.clone(), origin.clone());
        // The generated clients are Clone over a Clone channel: everything
        // below multiplexes over the single h2 connection made above.
        let mut shout_client = ShoutClient::with_origin(channel, origin);

        // Concurrent unary RPCs: h2 stream multiplexing under chaos. Each
        // seed interleaves the frames differently.
        let batch =
            (0..3u64).map(|seq| Self::echo_once(echo_client.clone(), time.clone(), round, seq));
        for outcome in futures::future::join_all(batch).await {
            outcome?;
        }

        Self::stream_once(echo_client, time.clone(), round).await?;
        Self::probe_unimplemented(&mut shout_client, &time).await?;
        Ok(())
    }

    async fn echo_once(
        mut client: EchoClient<H2Channel>,
        time: SimTimeProvider,
        round: u32,
        seq: u64,
    ) -> SimulationResult<()> {
        let text = format!("hello-{round}-{seq}");
        let mut request = Request::new(EchoRequest {
            text: text.clone(),
            seq,
        });
        let round_value = MetadataValue::try_from(round.to_string())
            .map_err(|e| SimulationError::InvalidState(format!("metadata value: {e}")))?;
        request
            .metadata_mut()
            .insert(ROUND_METADATA_KEY, round_value.clone());
        tracing::info!(round, seq, "sending echo rpc");

        match time.timeout(RPC_DEADLINE, client.echo(request)).await {
            Err(TimeError::Elapsed | TimeError::Shutdown) => {
                moonpool_sim::assert_sometimes!(true, "grpc_rpc_timed_out");
                Err(SimulationError::InvalidState("echo rpc timed out".into()))
            }
            Ok(Ok(response)) => {
                // Metadata must survive the full request/response round trip.
                moonpool_sim::assert_always!(
                    response.metadata().get(ROUND_METADATA_KEY) == Some(&round_value),
                    "response must echo request metadata"
                );
                let reply = response.into_inner();
                // What we sent must come back unchanged — end-to-end through
                // protobuf, gRPC framing, h2, and the simulated network.
                moonpool_sim::assert_always!(
                    reply.text == text && reply.seq == seq,
                    "echo reply must match request"
                );
                moonpool_sim::assert_sometimes!(true, "grpc_echo_succeeded");
                tracing::info!(round, seq, "echo rpc ok");
                Ok(())
            }
            Ok(Err(status)) if status.code() == Code::Unavailable => {
                // Server-side buggify — the RPC failed cleanly, the channel
                // stays usable.
                moonpool_sim::assert_sometimes!(true, "grpc_echo_unavailable");
                tracing::info!(round, seq, "echo unavailable (buggified server)");
                Ok(())
            }
            Ok(Err(status)) => Err(SimulationError::InvalidState(format!(
                "echo rpc failed: {status}"
            ))),
        }
    }

    async fn stream_once(
        mut client: EchoClient<H2Channel>,
        time: SimTimeProvider,
        round: u32,
    ) -> SimulationResult<()> {
        let text = format!("stream-{round}");
        let request = EchoStreamRequest {
            text: text.clone(),
            count: STREAM_COUNT,
        };
        tracing::info!(round, "starting echo stream");

        let mut stream = match time
            .timeout(RPC_DEADLINE, client.echo_stream(request))
            .await
        {
            Err(TimeError::Elapsed | TimeError::Shutdown) => {
                moonpool_sim::assert_sometimes!(true, "grpc_rpc_timed_out");
                return Err(SimulationError::InvalidState("stream rpc timed out".into()));
            }
            Ok(Ok(response)) => response.into_inner(),
            Ok(Err(status)) if status.code() == Code::Unavailable => {
                moonpool_sim::assert_sometimes!(true, "grpc_echo_unavailable");
                tracing::info!(round, "stream refused (buggified server)");
                return Ok(());
            }
            Ok(Err(status)) => {
                return Err(SimulationError::InvalidState(format!(
                    "stream rpc failed: {status}"
                )));
            }
        };

        let mut next_seq = 0u64;
        loop {
            match time.timeout(RPC_DEADLINE, stream.message()).await {
                Err(TimeError::Elapsed | TimeError::Shutdown) => {
                    moonpool_sim::assert_sometimes!(true, "grpc_rpc_timed_out");
                    return Err(SimulationError::InvalidState(
                        "stream item timed out".into(),
                    ));
                }
                Ok(Ok(Some(item))) => {
                    // In-order, gap-free delivery: h2 preserves stream order
                    // even while the sim chops the connection into partial
                    // reads and delayed segments.
                    moonpool_sim::assert_always!(
                        item.seq == next_seq && item.text == text,
                        "stream items must arrive in order and unchanged"
                    );
                    next_seq += 1;
                }
                Ok(Ok(None)) => {
                    // Clean end-of-stream: the server only ends early via an
                    // explicit error, so a clean end means full delivery.
                    moonpool_sim::assert_always!(
                        next_seq == STREAM_COUNT,
                        "clean stream end must deliver every item"
                    );
                    moonpool_sim::assert_sometimes!(true, "grpc_stream_completed");
                    tracing::info!(round, "stream completed");
                    return Ok(());
                }
                Ok(Err(status)) if status.code() == Code::Aborted => {
                    // Buggified mid-stream abort: partial delivery is fine —
                    // every item that did arrive was already validated above.
                    moonpool_sim::assert_sometimes!(true, "grpc_stream_aborted");
                    tracing::info!(round, delivered = next_seq, "stream aborted (buggified)");
                    return Ok(());
                }
                Ok(Err(status)) => {
                    return Err(SimulationError::InvalidState(format!(
                        "stream failed: {status}"
                    )));
                }
            }
        }
    }

    async fn probe_unimplemented(
        client: &mut ShoutClient<H2Channel>,
        time: &SimTimeProvider,
    ) -> SimulationResult<()> {
        tracing::info!("calling unmounted Shout service");

        let request = EchoRequest {
            text: "probe".to_string(),
            seq: 0,
        };
        match time.timeout(RPC_DEADLINE, client.shout(request)).await {
            Err(TimeError::Elapsed | TimeError::Shutdown) => {
                moonpool_sim::assert_sometimes!(true, "grpc_rpc_timed_out");
                Err(SimulationError::InvalidState("probe rpc timed out".into()))
            }
            Ok(Ok(_)) => {
                moonpool_sim::assert_always!(false, "unmounted service must not succeed");
                Ok(())
            }
            Ok(Err(status)) if status.code() == Code::Unimplemented => {
                moonpool_sim::assert_sometimes!(true, "grpc_unimplemented_detected");
                tracing::info!("unmounted service correctly rejected");
                Ok(())
            }
            Ok(Err(status)) => Err(SimulationError::InvalidState(format!(
                "probe rpc failed: {status}"
            ))),
        }
    }
}
