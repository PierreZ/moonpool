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
//! # Architecture
//!
//! - **[`proto`]**: protoc/prost-generated messages and stubs
//! - **[`ProviderExecutor`]**: `hyper::rt::Executor` over any `TaskProvider`
//! - **[`EchoProcess`]**: accepts TCP, serves the generated `EchoServer`
//!   over hyper h2
//! - **[`EchoWorkload`]**: drives unary RPCs through the generated
//!   `EchoClient`, validates echoes under chaos

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tonic::{Code, Request, Response, Status};

use moonpool_sim::{
    Detach, NetworkProvider, Process, SimContext, SimulationResult, TaskProvider, TcpListenerTrait,
    Workload,
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
use proto::{EchoRequest, EchoResponse};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

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

        let msg = request.into_inner();
        let served = self.served.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(seq = msg.seq, served, "echo_served");
        Ok(Response::new(EchoResponse {
            text: msg.text,
            seq: msg.seq,
        }))
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

/// Test driver that sends unary echo RPCs and validates responses.
pub struct EchoWorkload;

#[async_trait]
impl Workload for EchoWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server_ip = ctx.peer("grpc").ok_or_else(|| {
            moonpool_sim::SimulationError::InvalidState("grpc process not found".into())
        })?;
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
                    // Under chaos (connection resets, clogs, bit flips), RPCs
                    // fail. That's expected — we're testing resilience.
                    moonpool_sim::assert_sometimes!(true, "grpc_round_failed");
                    tracing::warn!(round, "round failed (expected under chaos): {e}");
                }
            }
        }

        tracing::info!("workload finished all rounds");
        Ok(())
    }
}

impl EchoWorkload {
    async fn run_round(ctx: &SimContext, server_ip: &str, round: u32) -> SimulationResult<()> {
        tracing::info!(round, "connecting to server");
        let stream = moonpool_sim::select! {
            biased;
            result = ctx.network().connect(server_ip) => result?,
            () = ctx.shutdown().cancelled() => return Ok(()),
        };

        let io = TokioIo::new(stream.compat());
        let executor = ProviderExecutor::new(ctx.task().clone());
        let (send_request, conn) = hyper::client::conn::http2::Builder::new(executor)
            .handshake(io)
            .await
            .map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("h2 handshake: {e}"))
            })?;
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
        let origin = format!("http://{server_ip}");
        let mut echo_client = EchoClient::with_origin(
            channel.clone(),
            http::Uri::try_from(&origin).map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("bad origin: {e}"))
            })?,
        );
        // Both generated clients multiplex over the same h2 connection.
        let mut shout_client = ShoutClient::with_origin(
            channel,
            http::Uri::try_from(&origin).map_err(|e| {
                moonpool_sim::SimulationError::InvalidState(format!("bad origin: {e}"))
            })?,
        );

        for seq in 0..3u64 {
            Self::echo_once(&mut echo_client, round, seq).await?;
        }
        Self::probe_unimplemented(&mut shout_client).await?;
        Ok(())
    }

    async fn echo_once(
        client: &mut EchoClient<H2Channel>,
        round: u32,
        seq: u64,
    ) -> SimulationResult<()> {
        let text = format!("hello-{round}-{seq}");
        tracing::info!(round, seq, "sending echo rpc");

        match client
            .echo(EchoRequest {
                text: text.clone(),
                seq,
            })
            .await
        {
            Ok(response) => {
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
            Err(status) if status.code() == Code::Unavailable => {
                // Server-side buggify — the RPC failed cleanly, the channel
                // stays usable.
                moonpool_sim::assert_sometimes!(true, "grpc_echo_unavailable");
                tracing::info!(round, seq, "echo unavailable (buggified server)");
                Ok(())
            }
            Err(status) => Err(moonpool_sim::SimulationError::InvalidState(format!(
                "echo rpc failed: {status}"
            ))),
        }
    }

    async fn probe_unimplemented(client: &mut ShoutClient<H2Channel>) -> SimulationResult<()> {
        tracing::info!("calling unmounted Shout service");

        match client
            .shout(EchoRequest {
                text: "probe".to_string(),
                seq: 0,
            })
            .await
        {
            Ok(_) => {
                moonpool_sim::assert_always!(false, "unmounted service must not succeed");
                Ok(())
            }
            Err(status) if status.code() == Code::Unimplemented => {
                moonpool_sim::assert_sometimes!(true, "grpc_unimplemented_detected");
                tracing::info!("unmounted service correctly rejected");
                Ok(())
            }
            Err(status) => Err(moonpool_sim::SimulationError::InvalidState(format!(
                "probe rpc failed: {status}"
            ))),
        }
    }
}
