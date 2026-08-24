//! Cross-run determinism regression for tonic over `ReconnectingChannel`.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use moonpool_hyper::{ChannelConfig, ChannelError, H2Server, ReconnectingChannel};
use moonpool_sim::{
    Attrition, AttritionScope, Chaos, ChaosMode, NetworkProvider, Process, SIM_FAULT_EVENT_NAME,
    SimContext, SimProviders, SimulationBuilder, SimulationError, SimulationResult, TaskProvider,
    TcpListenerTrait, Workload,
};
use moonpool_sim_examples::tonic_grpc::proto::echo_client::EchoClient;
use moonpool_sim_examples::tonic_grpc::proto::echo_server::{Echo, EchoServer};
use moonpool_sim_examples::tonic_grpc::proto::{EchoRequest, EchoResponse, EchoStreamRequest};
use moonpool_sim_examples::tonic_grpc::{EchoProcess, EchoWorkload};
use tonic::{Request, Response, Status};
use tower_service::Service;

type TraceEntry = (u64, u64, String, String, String);

const EVENT_NAMES: &[&str] = &[
    "grpc server listening",
    "accepted connection",
    "grpc server shutting down",
    "workload starting",
    "starting round",
    "round completed successfully",
    "workload finished all rounds",
    "echo_served",
    "echo_stream_started",
    "stream completed",
    "stream aborted (buggified)",
    "calling unmounted Shout service",
    "unmounted service correctly rejected",
    SIM_FAULT_EVENT_NAME,
];

const MULTI_CHANNEL_EVENT_NAMES: &[&str] = &[
    "multi_channel_server_listening",
    "multi_channel_connection_accepted",
    "multi_channel_echo_served",
    "multi_channel_response_received",
    "multi_channel_workload_finished",
];

type GrpcChannel = ReconnectingChannel<SimProviders, tonic::body::Body>;

#[derive(Clone)]
struct DateCheckingChannel {
    inner: GrpcChannel,
    date_seen: Arc<AtomicBool>,
}

impl Service<http::Request<tonic::body::Body>> for DateCheckingChannel {
    type Response = http::Response<hyper::body::Incoming>;
    type Error = ChannelError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Service::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let response = Service::call(&mut self.inner, request);
        let date_seen = Arc::clone(&self.date_seen);
        Box::pin(async move {
            let response = response.await?;
            date_seen.fetch_or(
                response.headers().contains_key(http::header::DATE),
                Ordering::Relaxed,
            );
            Ok(response)
        })
    }
}

#[derive(Default)]
struct DeterministicEcho;

#[tonic::async_trait]
impl Echo for DeterministicEcho {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        let request = request.into_inner();
        tracing::info!(seq = request.seq, "multi_channel_echo_served");
        Ok(Response::new(EchoResponse {
            text: request.text,
            seq: request.seq,
        }))
    }

    type EchoStreamStream = Pin<Box<dyn Stream<Item = Result<EchoResponse, Status>> + Send>>;

    async fn echo_stream(
        &self,
        _request: Request<EchoStreamRequest>,
    ) -> Result<Response<Self::EchoStreamStream>, Status> {
        Ok(Response::new(Box::pin(futures::stream::empty())))
    }
}

struct MultiChannelEchoProcess;

#[async_trait]
impl Process for MultiChannelEchoProcess {
    fn name(&self) -> &'static str {
        "multi-channel-grpc"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        let server = H2Server::new(ctx.providers());
        let service = EchoServer::new(DeterministicEcho);
        tracing::info!("multi_channel_server_listening");

        loop {
            moonpool_sim::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    tracing::info!("multi_channel_connection_accepted");
                    let connection = server.serve_connection_with_shutdown(
                        stream,
                        service.clone(),
                        ctx.shutdown().clone().cancelled_owned(),
                    );
                    ctx.task()
                        .spawn_task("multi-channel-grpc-server", async move {
                            if let Err(error) = connection.await {
                                tracing::debug!(%error, "multi-channel h2 connection ended");
                            }
                        })
                        .detach();
                }
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}

struct MultiChannelEchoWorkload;

#[async_trait]
impl Workload for MultiChannelEchoWorkload {
    fn name(&self) -> &'static str {
        "multi-channel-client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server_ips = ctx.topology().all_process_ips();
        if server_ips.len() != 3 {
            return Err(SimulationError::InvalidState(format!(
                "expected three gRPC servers, got {}",
                server_ips.len()
            )));
        }

        let mut channels = Vec::new();
        let mut lanes = Vec::new();
        for (server, address) in server_ips.iter().enumerate() {
            let origin = http::Uri::try_from(format!("http://{address}"))
                .map_err(|error| SimulationError::InvalidState(format!("bad origin: {error}")))?;
            for channel_index in 0..2_u64 {
                let channel = ReconnectingChannel::new(
                    ctx.providers(),
                    address.clone(),
                    ChannelConfig::default(),
                );
                let date_seen = Arc::new(AtomicBool::new(false));
                channels.push(channel.clone());
                let checked_channel = DateCheckingChannel {
                    inner: channel.clone(),
                    date_seen: Arc::clone(&date_seen),
                };
                // Establish every persistent connection before driving the
                // shared lanes concurrently. This mirrors a running cluster
                // after peer-channel startup without overflowing the
                // simulation listener's deliberately small pending backlog.
                let mut startup_error = None;
                for _ in 0..5 {
                    match Self::run_lane(
                        checked_channel.clone(),
                        origin.clone(),
                        server as u64,
                        channel_index,
                        9,
                        1,
                    )
                    .await
                    {
                        Ok(()) => {
                            startup_error = None;
                            break;
                        }
                        Err(error) => startup_error = Some(error),
                    }
                }
                if let Some(error) = startup_error {
                    return Err(error);
                }
                for lane in 0..2_u64 {
                    lanes.push(Self::run_lane(
                        checked_channel.clone(),
                        origin.clone(),
                        server as u64,
                        channel_index,
                        lane,
                        3,
                    ));
                }
            }
        }

        for result in futures::future::join_all(lanes).await {
            result?;
        }
        for channel in channels {
            channel.close();
        }
        tracing::info!("multi_channel_workload_finished");
        Ok(())
    }
}

impl MultiChannelEchoWorkload {
    async fn run_lane(
        channel: DateCheckingChannel,
        origin: http::Uri,
        server: u64,
        channel_index: u64,
        lane: u64,
        request_count: u64,
    ) -> SimulationResult<()> {
        let date_seen = Arc::clone(&channel.date_seen);
        let mut client = EchoClient::with_origin(channel, origin);
        for request_index in 0..request_count {
            let seq = server * 10_000 + channel_index * 1_000 + lane * 100 + request_index;
            let text = format!("server-{server}-channel-{channel_index}-lane-{lane}");
            let response = client
                .echo(Request::new(EchoRequest {
                    text: text.clone(),
                    seq,
                }))
                .await
                .map_err(|error| {
                    SimulationError::InvalidState(format!("gRPC echo failed: {error}"))
                })?;
            if date_seen.load(Ordering::Relaxed) {
                return Err(SimulationError::InvalidState(
                    "h2 response contained a wall-clock Date header".into(),
                ));
            }
            let response = response.into_inner();
            if response.seq != seq || response.text != text {
                return Err(SimulationError::InvalidState(
                    "gRPC echo response did not match its request".into(),
                ));
            }
            tracing::info!(
                server,
                channel = channel_index,
                lane,
                request = request_index,
                "multi_channel_response_received"
            );
        }
        Ok(())
    }
}

fn run_once(seed: u64) -> Vec<TraceEntry> {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&trace);

    let report = SimulationBuilder::new()
        .processes(1, || Box::new(EchoProcess))
        .workload(EchoWorkload)
        .invariant_fn("tonic replay trace", move |query, _| {
            let mut events = EVENT_NAMES
                .iter()
                .flat_map(|name| query.snapshot(name))
                .map(|event| {
                    (
                        event.seq,
                        event.time_ms,
                        event.source,
                        event.name,
                        format!("{:?}", event.fields),
                    )
                })
                .collect::<Vec<_>>();
            events.sort_by_key(|event| event.0);
            *captured
                .lock()
                .expect("Mutex poisoned: prior test panicked") = events;
        })
        .enable_chaos([
            Chaos::Network(ChaosMode::Random),
            Chaos::Attrition {
                config: Attrition {
                    max_dead: 1,
                    prob_graceful: 0.3,
                    prob_crash: 0.5,
                    prob_wipe: 0.2,
                    recovery_delay_ms: None,
                    grace_period_ms: None,
                    scope: AttritionScope::PerProcess,
                },
                mode: ChaosMode::Random,
            },
        ])
        .chaos_duration(Duration::from_secs(10))
        .set_debug_seeds(vec![seed])
        .set_iterations(1)
        .run();
    assert_eq!(report.failed_runs, 0, "seed {seed} must succeed");

    let events = trace
        .lock()
        .expect("Mutex poisoned: prior test panicked")
        .clone();
    assert!(!events.is_empty(), "seed {seed} must emit trace events");
    events
}

fn run_multi_channel_once(seed: u64) -> Vec<TraceEntry> {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&trace);

    let report = SimulationBuilder::new()
        .processes(3, || Box::new(MultiChannelEchoProcess))
        .workload(MultiChannelEchoWorkload)
        .invariant_fn("multi-channel tonic replay trace", move |query, _| {
            let mut events = MULTI_CHANNEL_EVENT_NAMES
                .iter()
                .flat_map(|name| query.snapshot(name))
                .map(|event| {
                    (
                        event.seq,
                        event.time_ms,
                        event.source,
                        event.name,
                        format!("{:?}", event.fields),
                    )
                })
                .collect::<Vec<_>>();
            events.sort_by_key(|event| event.0);
            *captured
                .lock()
                .expect("Mutex poisoned: prior test panicked") = events;
        })
        .set_debug_seeds(vec![seed])
        .set_iterations(1)
        .run();
    assert_eq!(
        report.failed_runs, 0,
        "seed {seed} must succeed:\n{report}\n{:?}",
        report.individual_metrics
    );

    let events = trace
        .lock()
        .expect("Mutex poisoned: prior test panicked")
        .clone();
    assert!(
        events
            .iter()
            .any(|event| event.3 == "multi_channel_workload_finished"),
        "seed {seed} must finish the multi-channel workload"
    );
    events
}

#[test]
fn tonic_replay_is_independent_of_earlier_in_process_runs() {
    for seed in [1_u64, 42] {
        assert_eq!(run_once(seed), run_once(seed), "warm-up seed {seed}");
    }

    assert_eq!(run_once(12_345), run_once(12_345), "target seed");
}

#[test]
fn multi_channel_grpc_replays_without_wall_clock_headers() {
    for seed in [1_u64, 42, 12_345] {
        assert_eq!(
            run_multi_channel_once(seed),
            run_multi_channel_once(seed),
            "multi-channel seed {seed}"
        );
    }
}
