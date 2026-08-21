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
//! stubs, and lets `moonpool-hyper` drive hyper over the provider traits:
//!
//! - **Client**: one [`ReconnectingChannel`] for the whole workload, the role
//!   `tonic::transport::Channel` plays in production. It connects lazily,
//!   reconnects after the server is killed and restarted, and multiplexes
//!   every round's RPCs over one h2 connection.
//! - **Server**: [`H2Server::serve_connection_with_shutdown`] per accepted
//!   connection, wired to the process shutdown token so a graceful Attrition
//!   reboot drains in-flight RPCs instead of resetting them.
//! - **IO, spawning, timers**: all inside moonpool-hyper. The stream goes
//!   straight into hyper's IO traits (no `Compat` plus `TokioIo` bridge),
//!   hyper's internal h2 tasks land on the deterministic sim executor, and h2
//!   keepalive ping/pong runs on sim time.
//! - **gRPC**: the generated `EchoServer`/`EchoClient` from
//!   `proto/echo.proto` — the exact same codegen output production uses.
//!
//! Unlike hyper's HTTP/1 connection state machine (which is `!Send` and must
//! be driven inline — see `axum_web.rs`), h2 connections are `Send`, so the
//! server connection futures are spawned as ordinary sim tasks, and the
//! channel spawns its own connection task.
//!
//! What the workload exercises per round, all over the shared channel:
//!
//! - **Concurrent unary RPCs** (cloned clients, joined futures) with
//!   metadata round-tripping and per-call deadlines via the time provider
//! - **Server streaming** with in-order delivery checks and buggified
//!   mid-stream aborts
//! - **Unknown-service probe** against the unmounted `Shout` service
//!   (expects `UNIMPLEMENTED`)
//! - **Transport failures** from the chaos underneath: a dead server or a
//!   connection killed mid-RPC arrives as `Code::Unknown`, since tonic's
//!   h2-aware `Status` mapping lives behind features this example does not
//!   enable. Rounds fail, the channel reconnects, later rounds succeed.
//!
//! The sim binary adds Attrition chaos (server crash/reboot) on top of the
//! default network chaos, so rounds also see dead servers and reconnects.
//!
//! # Architecture
//!
//! - **[`proto`]**: protoc/prost-generated messages and stubs
//! - **[`EchoProcess`]**: accepts TCP, serves the generated `EchoServer`
//!   over hyper h2
//! - **[`EchoWorkload`]**: drives the RPC mix, validates responses under
//!   chaos

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use tonic::metadata::MetadataValue;
use tonic::{Code, Request, Response, Status};

use moonpool_hyper::{ChannelConfig, H2Server, H2ServerConfig, KeepAlive, ReconnectingChannel};
use moonpool_sim::{
    NetworkProvider, Process, SimContext, SimProviders, SimTimeProvider, SimulationError,
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

/// The channel every round shares: one h2 connection to the gRPC server,
/// rebuilt by moonpool-hyper whenever chaos takes it down.
type GrpcChannel = ReconnectingChannel<SimProviders, tonic::body::Body>;

/// Metadata key the client stamps on each request; the server must echo it
/// back on the response.
const ROUND_METADATA_KEY: &str = "x-moonpool-round";

/// Per-RPC deadline. Generous against ordinary chaos delays, short enough to
/// cut through clogged connections and dead servers — and shorter than the
/// keepalive detection window (interval + timeout), so a mid-RPC clog trips
/// the deadline while keepalive covers idle connections.
const RPC_DEADLINE: Duration = Duration::from_secs(4);

/// Items requested per streaming call.
const STREAM_COUNT: u64 = 4;

/// h2 keepalive ping interval. Hyper reads the clock through moonpool-hyper's
/// timer, so pings fire on deterministic sim time; a clogged connection that
/// swallows the ping ACK longer than [`KEEP_ALIVE_TIMEOUT`] is torn down.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(3);

/// How long hyper waits for a keepalive ACK before declaring the
/// connection dead.
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Budget for one of the channel's connect attempts.
///
/// Deliberately shorter than [`RPC_DEADLINE`] so that a dead server surfaces
/// as the channel's own connect timeout, which the caller sees as a
/// `Code::Unknown` status, rather than as the caller's deadline every time.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The keepalive both sides run, on sim time.
///
/// A clogged connection that swallows the ping ACK longer than the timeout is
/// torn down by hyper, which is what gives the channel something to reconnect
/// from when the network misbehaves without closing the socket.
fn keep_alive() -> KeepAlive {
    KeepAlive {
        interval: KEEP_ALIVE_INTERVAL,
        timeout: KEEP_ALIVE_TIMEOUT,
        // Client-side only, and off: an idle connection between rounds should
        // not be torn down for being idle. Ignored by the server side.
        while_idle: false,
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
        let server = H2Server::new(ctx.providers()).with_config(H2ServerConfig {
            keep_alive: Some(keep_alive()),
            // The sim delivers each IoSlice separately, with its own chance of
            // chaos; that path is unreachable while the IO layer reports no
            // vectored support.
            vectored_writes: true,
        });
        tracing::info!("grpc server listening");

        loop {
            moonpool_sim::select! {
                accept = listener.accept() => {
                    let (stream, addr) = accept?;
                    tracing::info!(%addr, "accepted connection");

                    // Wired to the shutdown token: a graceful Attrition reboot
                    // signals it, and hyper then finishes the RPCs already in
                    // flight before the connection ends.
                    let shutdown = ctx.shutdown().clone();
                    let connection = server.serve_connection_with_shutdown(
                        stream,
                        echo.clone(),
                        shutdown.clone().cancelled_owned(),
                    );

                    // Unlike hyper's HTTP/1 connection (which is !Send and must
                    // be driven inline), the h2 connection future is Send, so it
                    // runs as an ordinary spawned sim task.
                    ctx.task()
                        .spawn_task("grpc-server-conn", async move {
                            let outcome = connection.await;
                            if shutdown.is_cancelled() {
                                // The connection outlived a shutdown signal, so
                                // the graceful drain ran rather than the socket
                                // being dropped from under the client.
                                moonpool_sim::assert_sometimes!(
                                    true,
                                    "grpc_server_drained_on_shutdown"
                                );
                            }
                            if let Err(e) = outcome {
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

        // One channel for the whole workload, exactly as a production tonic
        // client would hold one `Channel`. It connects on the first RPC and
        // rebuilds itself after every server death, so the rounds below
        // exercise reconnection rather than reconnecting by hand.
        let channel: GrpcChannel = ReconnectingChannel::new(
            ctx.providers(),
            server_ip.clone(),
            ChannelConfig {
                connection_timeout: CONNECT_TIMEOUT,
                keep_alive: Some(keep_alive()),
                vectored_writes: true,
                ..ChannelConfig::default()
            },
        );
        let origin = http::Uri::try_from(format!("http://{server_ip}"))
            .map_err(|e| SimulationError::InvalidState(format!("bad origin: {e}")))?;

        let mut failed_before = false;
        for round in 0..5u32 {
            tracing::info!(round, "starting round");
            let result = moonpool_sim::select! {
                biased;
                result = Self::run_round(ctx, &channel, &origin, round) => result,
                () = ctx.shutdown().cancelled() => {
                    tracing::info!(round, "shutdown during round, exiting");
                    break;
                }
            };
            match result {
                Ok(()) => {
                    if failed_before {
                        // The channel produced a working connection after an
                        // earlier round had lost one (or failed to get one):
                        // reconnection, observed end to end through gRPC.
                        moonpool_sim::assert_sometimes!(true, "grpc_channel_recovered");
                    }
                    tracing::info!(round, "round completed successfully");
                }
                Err(e) => {
                    // Under chaos (connection resets, clogs, bit flips, server
                    // reboots), RPCs fail. That's expected — we're testing
                    // resilience.
                    failed_before = true;
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
    async fn run_round(
        ctx: &SimContext,
        channel: &GrpcChannel,
        origin: &http::Uri,
        round: u32,
    ) -> SimulationResult<()> {
        let time = ctx.time().clone();

        // No connect, no handshake, no connection task here: the channel owns
        // all of that and establishes a connection on the first RPC below.
        let echo_client = EchoClient::with_origin(channel.clone(), origin.clone());
        // The generated clients are Clone over a Clone channel: everything
        // below multiplexes over the channel's single h2 connection.
        let mut shout_client = ShoutClient::with_origin(channel.clone(), origin.clone());

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
        mut client: EchoClient<GrpcChannel>,
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
            Ok(Err(status)) if status.code() == Code::Unknown => {
                // The transport failed under us: no connection to be had, or
                // one that died mid-RPC. tonic reports it as Unknown because
                // its h2-aware Status mapping is behind features this example
                // does not enable. The round fails; the channel reconnects on
                // its own and a later round proves it.
                moonpool_sim::assert_sometimes!(true, "grpc_transport_failed");
                tracing::warn!(round, seq, "echo transport failure: {status}");
                Err(SimulationError::InvalidState(format!(
                    "echo rpc transport failure: {status}"
                )))
            }
            Ok(Err(status)) => Err(SimulationError::InvalidState(format!(
                "echo rpc failed: {status}"
            ))),
        }
    }

    async fn stream_once(
        mut client: EchoClient<GrpcChannel>,
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
            Ok(Err(status)) if status.code() == Code::Unknown => {
                moonpool_sim::assert_sometimes!(true, "grpc_transport_failed");
                return Err(SimulationError::InvalidState(format!(
                    "stream rpc transport failure: {status}"
                )));
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
                Ok(Err(status)) if status.code() == Code::Unknown => {
                    // The connection died with items still to come. Whatever
                    // arrived was validated above, so this is a clean loss.
                    moonpool_sim::assert_sometimes!(true, "grpc_transport_failed");
                    return Err(SimulationError::InvalidState(format!(
                        "stream transport failure after {next_seq} items: {status}"
                    )));
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
        client: &mut ShoutClient<GrpcChannel>,
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
            Ok(Err(status)) if status.code() == Code::Unknown => {
                moonpool_sim::assert_sometimes!(true, "grpc_transport_failed");
                Err(SimulationError::InvalidState(format!(
                    "probe rpc transport failure: {status}"
                )))
            }
            Ok(Err(status)) => Err(SimulationError::InvalidState(format!(
                "probe rpc failed: {status}"
            ))),
        }
    }
}
