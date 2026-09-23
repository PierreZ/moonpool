//! Saturation measurements behind the stream and batch defaults.
//!
//! Real TCP on localhost, production providers, a multi-thread Tokio
//! runtime. Three measurements, printed as tables:
//!
//! 1. **Stream throughput by window**: one stream of 4 KiB items consumed
//!    eagerly, for windows from 16 KiB to 4 MiB. The default
//!    ([`StreamPolicy::window_bytes`](moonpool_rpc::StreamPolicy)) should
//!    sit where throughput stops growing.
//! 2. **Unary latency beside saturated streams, by batch size**: eight
//!    streams of 16 KiB items with 1 MiB windows keep the session's writer
//!    busy while unary calls measure their round trip, for several
//!    [`ResourceLimits::max_frames_per_batch`](moonpool_rpc::ResourceLimits)
//!    values.
//! 3. **Push-back**: a burst of 4096 unary calls against a server whose
//!    handler is slow; how many are refused `Overloaded` before admission
//!    with the default in-flight and queue budgets, and that the session
//!    survives.
//!
//! Run with `cargo run --release -p moonpool-rpc --example stream_saturation`.
//! Numbers depend on the host; the book records one run.

use std::time::{Duration, Instant};

use futures::StreamExt;
use moonpool_core::TokioProviders;
use moonpool_rpc::{
    AccessClass, ErrorReason, IncomingRequest, MethodId, RequestStream, ResourceLimits, RpcConfig,
    RpcDriver, RpcHandle, RpcMethod, SchemaVersion, ServiceRef, StreamPolicy,
};

#[derive(Clone, PartialEq, prost::Message)]
struct Fill {
    #[prost(uint32, tag = "1")]
    item_bytes: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Blob {
    #[prost(bytes = "vec", tag = "1")]
    data: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Tick {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(uint32, tag = "2")]
    hold_ms: u32,
}

struct Firehose;
impl RpcMethod for Firehose {
    type Request = Fill;
    type Reply = Blob;
    const METHOD: MethodId = MethodId::new(0xF1);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "firehose";
    const STREAMING: bool = true;
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Tick;
    type Reply = Tick;
    const METHOD: MethodId = MethodId::new(0xF2);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

/// Stream items until the consumer goes away.
fn serve_firehose(mut requests: RequestStream<Firehose>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = requests.recv().await {
            tokio::spawn(async move {
                let Ok(producer) = reply.into_stream() else {
                    return;
                };
                let blob = Blob {
                    data: vec![7; request.item_bytes as usize],
                };
                while producer.send(&blob).await.is_ok() {}
            });
        }
    })
}

fn serve_echo(mut requests: RequestStream<Echo>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = requests.recv().await {
            tokio::spawn(async move {
                if request.hold_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(u64::from(request.hold_ms))).await;
                }
                let _ = reply.send(&request);
            });
        }
    })
}

struct Pair {
    client: RpcHandle<TokioProviders>,
    firehose: ServiceRef<Firehose>,
    echo: ServiceRef<Echo>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    drivers: Vec<tokio::task::JoinHandle<std::io::Error>>,
}

impl Pair {
    async fn new(server: RpcConfig, client: RpcConfig) -> std::io::Result<Self> {
        let (server_driver, server_rpc) =
            RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", server).await?;
        let (client_driver, client_rpc) = RpcDriver::client_only(TokioProviders::new(), client)?;
        let registered = |error: moonpool_rpc::RpcError| std::io::Error::other(error.to_string());
        let (firehose, streams) = server_rpc
            .register::<Firehose>(AccessClass::Public)
            .map_err(registered)?;
        let (echo, echoes) = server_rpc
            .register::<Echo>(AccessClass::Public)
            .map_err(registered)?;
        Ok(Self {
            client: client_rpc,
            firehose,
            echo,
            tasks: vec![serve_firehose(streams), serve_echo(echoes)],
            drivers: vec![
                tokio::spawn(server_driver.run()),
                tokio::spawn(client_driver.run()),
            ],
        })
    }

    fn stop(self) {
        for task in self.tasks {
            task.abort();
        }
        for driver in self.drivers {
            driver.abort();
        }
    }
}

/// Items and bytes taken from one eagerly consumed stream in `budget`.
async fn throughput(window: u64, budget: Duration) -> std::io::Result<f64> {
    let pair = Pair::new(RpcConfig::default(), RpcConfig::default()).await?;
    let mut stream = pair
        .firehose
        .bind(&pair.client)
        .get_reply_stream_with_window(&Fill { item_bytes: 4096 }, window)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let started = Instant::now();
    let mut bytes = 0usize;
    while started.elapsed() < budget {
        match stream.next().await {
            Some(Ok(blob)) => bytes += blob.data.len(),
            _ => break,
        }
    }
    drop(stream);
    pair.stop();
    let kib = f64::from(u32::try_from(bytes >> 10).unwrap_or(u32::MAX));
    Ok(kib / 1024.0 / started.elapsed().as_secs_f64())
}

/// Unary round trips (p50, p99, max) beside eight saturating streams.
async fn latency_beside_streams(batch: usize) -> std::io::Result<(Duration, Duration, Duration)> {
    let config = RpcConfig {
        limits: ResourceLimits {
            max_frames_per_batch: batch,
            ..ResourceLimits::default()
        },
        ..RpcConfig::default()
    };
    let pair = Pair::new(config.clone(), config).await?;
    let client = pair.firehose.bind(&pair.client);
    let mut streams = Vec::new();
    for _ in 0..8 {
        streams.push(
            client
                .get_reply_stream_with_window(
                    &Fill {
                        item_bytes: 16 * 1024,
                    },
                    1 << 20,
                )
                .map_err(|error| std::io::Error::other(error.to_string()))?,
        );
    }
    // Consumers that keep every window busy.
    let consumers: Vec<_> = streams
        .into_iter()
        .map(|mut stream| {
            tokio::spawn(async move { while let Some(Ok(_)) = stream.next().await {} })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let echo = pair.echo.bind(&pair.client);
    let mut samples = Vec::new();
    for id in 0..300 {
        let started = Instant::now();
        let _ = echo
            .try_get_reply_within(&Tick { id, hold_ms: 0 }, Duration::from_secs(5))
            .await;
        samples.push(started.elapsed());
    }
    for consumer in consumers {
        consumer.abort();
    }
    pair.stop();
    samples.sort();
    let at = |q: usize| samples[(samples.len() - 1) * q / 100];
    Ok((at(50), at(99), at(100)))
}

/// A burst of slow calls: (admitted, refused overloaded, other errors,
/// connections left).
async fn push_back() -> std::io::Result<(usize, usize, usize, usize)> {
    let pair = Pair::new(RpcConfig::default(), RpcConfig::default()).await?;
    let echo = pair.echo.bind(&pair.client);
    let calls = (0..4096).map(|id| {
        let echo = echo.clone();
        async move {
            echo.try_get_reply_within(&Tick { id, hold_ms: 200 }, Duration::from_secs(10))
                .await
        }
    });
    let outcomes = futures::future::join_all(calls).await;
    let refused = outcomes
        .iter()
        .filter(
            |outcome| matches!(outcome, Err(error) if *error.reason() == ErrorReason::Overloaded),
        )
        .count();
    let admitted = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    let connections = pair.client.stats().map_or(0, |stats| stats.connections);
    pair.stop();
    Ok((
        admitted,
        refused,
        outcomes.len() - admitted - refused,
        connections,
    ))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> std::io::Result<()> {
    println!("| window | MiB/s (4 KiB items) |");
    println!("|---|---|");
    for window in [16u64 << 10, 64 << 10, 256 << 10, 1 << 20, 4 << 20] {
        let rate = throughput(window, Duration::from_millis(1500)).await?;
        println!("| {} KiB | {rate:.0} |", window >> 10);
    }
    println!();
    println!("| max_frames_per_batch | unary p50 | p99 | max (8 saturating streams) |");
    println!("|---|---|---|---|");
    for batch in [1, 16, 64, 256] {
        let (p50, p99, max) = latency_beside_streams(batch).await?;
        println!("| {batch} | {p50:?} | {p99:?} | {max:?} |");
    }
    println!();
    let (admitted, refused, other, connections) = push_back().await?;
    let defaults = RpcConfig::default();
    println!(
        "push-back: 4096 slow calls (in-flight budget {}, endpoint queue {}): \
         {admitted} admitted, {refused} refused Overloaded before admission, \
         {other} other errors, {connections} connection(s) left open",
        defaults.limits.max_inflight_per_connection, defaults.endpoint_queue_capacity
    );
    let policy = StreamPolicy::default();
    println!(
        "defaults: window {} KiB, max window {} MiB, streams/connection {}",
        policy.window_bytes >> 10,
        policy.max_window_bytes >> 20,
        policy.max_streams_per_connection
    );
    Ok(())
}
