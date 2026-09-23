//! Real-network soak and performance of moonpool-rpc over localhost TCP,
//! on both Tokio runtime flavors: open-loop unary load at declared offered
//! rates and payloads, reply-stream throughput by stream count and window,
//! an overload burst and its recovery, and the resources the runtimes hold
//! throughout (queued bytes, owed replies, pending calls, connections,
//! driver tasks and live heap), sampled every 100 ms.
//!
//! Latencies are measured from each call's *scheduled* start (open loop,
//! no coordinated omission). Every scenario ends by dropping its runtimes
//! and requires every resource probe back at its baseline
//! (`ResourceProbe::is_at_baseline`). `--smoke` runs every scenario briefly
//! at low rates and checks only the invariants (CI); the full run prints
//! the tables recorded in `docs/plans/moonpool-rpc/qualification.md`.
//!
//! ```text
//! cargo run --release -p moonpool-rpc-sim --example rpc_soak -- [--smoke] \
//!     [--flavor current|multi|both] [--seconds N]
//! ```
//!
//! A calibration point on the machine it runs on, not a benchmark suite.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use moonpool_core::TokioProviders;
use moonpool_rpc::{
    AccessClass, ErrorReason, IncomingRequest, MethodId, ResourceProbe, RpcConfig, RpcDriver,
    RpcHandle, RpcMethod, SchemaVersion, ServiceRef,
};

/// The system allocator, tracking live heap bytes and their peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded unchanged to the system allocator; the
// only addition is atomic accounting of the sizes involved.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        // SAFETY: the caller's contract is System's contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from this allocator, i.e. from System.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size >= layout.size() {
            let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed) + new_size
                - layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        } else {
            LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract is System's contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn mib(bytes: usize) -> f64 {
    f64::from(u32::try_from(bytes / 1024).unwrap_or(u32::MAX)) / 1024.0
}

/// A request with a payload of the scenario's size.
#[derive(Clone, PartialEq, prost::Message)]
struct Blob {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(bytes = "vec", tag = "2")]
    payload: Vec<u8>,
    /// Hold the reply this long (overload scenario), in milliseconds.
    #[prost(uint32, tag = "3")]
    hold_ms: u32,
}

/// A reply, or a stream item.
#[derive(Clone, PartialEq, prost::Message)]
struct Ack {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(bytes = "vec", tag = "2")]
    payload: Vec<u8>,
}

struct Soak;
impl RpcMethod for Soak {
    type Request = Blob;
    type Reply = Ack;
    const METHOD: MethodId = MethodId::new(0x0219_5001);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "soak.unary";
}

struct Flow;
impl RpcMethod for Flow {
    type Request = Blob;
    type Reply = Ack;
    const METHOD: MethodId = MethodId::new(0x0219_5002);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "soak.stream";
    const STREAMING: bool = true;
}

type Error = Box<dyn std::error::Error + Send + Sync>;

/// A server and a client runtime on one session, with their probes.
struct Pair {
    server: RpcHandle<TokioProviders>,
    client: RpcHandle<TokioProviders>,
    unary: ServiceRef<Soak>,
    flow: ServiceRef<Flow>,
    probes: [ResourceProbe; 2],
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Pair {
    async fn start() -> Result<Self, Error> {
        let (server_driver, server) =
            RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", RpcConfig::default()).await?;
        let (client_driver, client) =
            RpcDriver::client_only(TokioProviders::new(), RpcConfig::default())?;
        let probes = [
            server.probe().ok_or("server gone")?,
            client.probe().ok_or("client gone")?,
        ];
        let (unary, mut unary_stream) = server.register::<Soak>(AccessClass::Public)?;
        let (flow, mut flow_stream) = server.register::<Flow>(AccessClass::Public)?;
        let tasks = vec![
            tokio::spawn(async move {
                let _ = server_driver.run().await;
            }),
            tokio::spawn(async move {
                let _ = client_driver.run().await;
            }),
            tokio::spawn(async move {
                while let Some(IncomingRequest { request, reply }) = unary_stream.recv().await {
                    if request.hold_ms == 0 {
                        let _ = reply.send(&Ack {
                            id: request.id,
                            payload: Vec::new(),
                        });
                    } else {
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(u64::from(request.hold_ms)))
                                .await;
                            let _ = reply.send(&Ack {
                                id: request.id,
                                payload: Vec::new(),
                            });
                        });
                    }
                }
            }),
            tokio::spawn(async move {
                while let Some(IncomingRequest { request, reply }) = flow_stream.recv().await {
                    let Ok(producer) = reply.into_stream() else {
                        continue;
                    };
                    tokio::spawn(async move {
                        let item = Ack {
                            id: request.id,
                            payload: request.payload,
                        };
                        loop {
                            if producer.send(&item).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            }),
        ];
        Ok(Self {
            server,
            client,
            unary,
            flow,
            probes,
            tasks,
        })
    }

    /// Drop both runtimes; every probe must return to its baseline.
    async fn finish(self) -> bool {
        let Self {
            server,
            client,
            unary,
            flow,
            probes,
            tasks,
        } = self;
        drop((server, client, unary, flow));
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        // Handler tasks spawned per request finish on their own.
        for _ in 0..100 {
            if probes.iter().all(ResourceProbe::is_at_baseline) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for probe in &probes {
            eprintln!("not at baseline: {probe:?} {:?}", probe.outstanding());
        }
        false
    }
}

/// Peak resources seen while a scenario ran.
#[derive(Default, Clone, Copy)]
struct Peaks {
    queued_bytes: u64,
    inflight: usize,
    pending: usize,
    tasks: usize,
    heap: usize,
}

/// Sample both runtimes every 100 ms until `stop`.
async fn sample(pair: &Pair, stop: &AtomicBool) -> Peaks {
    let mut peaks = Peaks::default();
    while !stop.load(Ordering::Relaxed) {
        for handle in [&pair.server, &pair.client] {
            if let Some(stats) = handle.stats() {
                peaks.queued_bytes = peaks.queued_bytes.max(stats.queued_bytes);
                peaks.inflight = peaks.inflight.max(stats.inflight_requests);
                peaks.pending = peaks.pending.max(stats.pending_calls);
            }
        }
        peaks.tasks = peaks
            .tasks
            .max(pair.probes.iter().map(ResourceProbe::live_tasks).sum());
        peaks.heap = peaks.heap.max(LIVE.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    peaks
}

fn percentile(samples: &mut [Duration], per_mille: usize) -> Duration {
    samples.sort_unstable();
    samples
        .get(samples.len().saturating_sub(1) * per_mille / 1000)
        .copied()
        .unwrap_or_default()
}

/// Open-loop unary load: `rate` calls per second of `payload` bytes for
/// `seconds`, at most `concurrency` in flight (beyond it the client sheds).
struct UnaryLoad {
    rate: u64,
    payload: usize,
    concurrency: usize,
}

struct UnaryResult {
    offered: u64,
    completed: u64,
    failed: u64,
    shed: u64,
    elapsed: Duration,
    samples: Vec<Duration>,
    peaks: Peaks,
}

async fn unary_load(pair: &Pair, load: &UnaryLoad, seconds: u64) -> UnaryResult {
    let client = pair.unary.bind(&pair.client);
    let payload = vec![0x5a; load.payload];
    let in_flight = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let total = load.rate * seconds;
    let interval = Duration::from_secs(1).as_nanos() / u128::from(load.rate.max(1));
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let generator = async {
        let mut shed = 0u64;
        for id in 0..total {
            let due = started
                + Duration::from_nanos(u64::try_from(u128::from(id) * interval).unwrap_or(0));
            tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
            if in_flight.load(Ordering::Relaxed) >= load.concurrency {
                shed += 1;
                continue;
            }
            in_flight.fetch_add(1, Ordering::Relaxed);
            let client = client.clone();
            let request = Blob {
                id,
                payload: payload.clone(),
                hold_ms: 0,
            };
            let in_flight = Arc::clone(&in_flight);
            let failed = Arc::clone(&failed);
            let sender = sender.clone();
            tokio::spawn(async move {
                let outcome = client.try_get_reply(&request).await;
                in_flight.fetch_sub(1, Ordering::Relaxed);
                match outcome {
                    Ok(_) => {
                        let _ = sender.send(due.elapsed());
                    }
                    Err(_) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
        while in_flight.load(Ordering::Relaxed) > 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        stop.store(true, Ordering::Relaxed);
        shed
    };
    let (shed, peaks) = tokio::join!(generator, sample(pair, &stop));
    let elapsed = started.elapsed();
    drop(sender);
    let mut samples = Vec::new();
    while let Some(sample) = receiver.recv().await {
        samples.push(sample);
    }
    UnaryResult {
        offered: total,
        completed: samples.len() as u64,
        failed: failed.load(Ordering::Relaxed),
        shed,
        elapsed,
        samples,
        peaks,
    }
}

/// `streams` concurrent reply streams of 1 KiB items under `window`;
/// returns the consumed bytes per second.
async fn stream_load(pair: &Pair, streams: usize, window: u64, seconds: u64) -> (f64, Peaks) {
    let client = pair.flow.bind(&pair.client);
    let consumed = Arc::new(AtomicU64::new(0));
    let stop = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let readers = (0..streams).map(|index| {
        let client = client.clone();
        let consumed = Arc::clone(&consumed);
        async move {
            let request = Blob {
                id: index as u64,
                payload: vec![0xa5; 1024],
                hold_ms: 0,
            };
            let Ok(mut stream) = client.get_reply_stream_with_window(&request, window) else {
                return;
            };
            while Instant::now() < deadline {
                match stream.recv().await {
                    Some(Ok(item)) => {
                        consumed.fetch_add(item.payload.len() as u64, Ordering::Relaxed);
                    }
                    _ => return,
                }
            }
        }
    });
    let run = async {
        futures::future::join_all(readers).await;
        stop.store(true, Ordering::Relaxed);
    };
    let started = Instant::now();
    let ((), peaks) = tokio::join!(run, sample(pair, &stop));
    let rate = f64::from(u32::try_from(consumed.load(Ordering::Relaxed)).unwrap_or(u32::MAX))
        / started.elapsed().as_secs_f64();
    (rate, peaks)
}

/// A burst of `calls` held calls at once (twice the default per-connection
/// in-flight budget and more), then a probe call: how many were refused
/// `Overloaded`, and how long until the session served again.
async fn overload(pair: &Pair, calls: u64) -> (u64, u64, Duration, Peaks) {
    let client = pair.unary.bind(&pair.client);
    let stop = AtomicBool::new(false);
    let burst = async {
        let outcomes = futures::future::join_all((0..calls).map(|id| {
            let client = client.clone();
            async move {
                client
                    .try_get_reply(&Blob {
                        id,
                        payload: Vec::new(),
                        hold_ms: 200,
                    })
                    .await
            }
        }))
        .await;
        let refused = outcomes
            .iter()
            .filter(|outcome| {
                matches!(outcome, Err(error) if *error.reason() == ErrorReason::Overloaded)
            })
            .count() as u64;
        let served = outcomes.iter().filter(|outcome| outcome.is_ok()).count() as u64;
        let recovery = Instant::now();
        let mut recovered = Duration::MAX;
        for _ in 0..1000 {
            if client
                .try_get_reply(&Blob {
                    id: u64::MAX,
                    payload: Vec::new(),
                    hold_ms: 0,
                })
                .await
                .is_ok()
            {
                recovered = recovery.elapsed();
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        stop.store(true, Ordering::Relaxed);
        (refused, served, recovered)
    };
    let ((refused, served, recovered), peaks) = tokio::join!(burst, sample(pair, &stop));
    (refused, served, recovered, peaks)
}

struct Options {
    smoke: bool,
    seconds: u64,
    flavors: Vec<&'static str>,
}

fn options() -> Options {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let smoke = arguments.iter().any(|argument| argument == "--smoke");
    let value = |flag: &str| {
        arguments
            .iter()
            .position(|argument| argument == flag)
            .and_then(|index| arguments.get(index + 1))
            .cloned()
    };
    let seconds = value("--seconds")
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or(if smoke { 1 } else { 5 });
    let flavors = match value("--flavor").as_deref() {
        Some("current") => vec!["current"],
        Some("multi") => vec!["multi"],
        _ => vec!["current", "multi"],
    };
    Options {
        smoke,
        seconds,
        flavors,
    }
}

async fn soak(options: &Options, flavor: &str) -> Result<bool, Error> {
    let mut ok = soak_unary(options, flavor).await?;
    ok &= soak_streams(options, flavor).await?;
    ok &= soak_overload(options, flavor).await?;
    println!(
        "\nlive heap after the {flavor}-thread runs: {:.1} MiB (peak {:.1} MiB)",
        mib(LIVE.load(Ordering::Relaxed)),
        mib(PEAK.load(Ordering::Relaxed))
    );
    Ok(ok)
}

async fn soak_unary(options: &Options, flavor: &str) -> Result<bool, Error> {
    let mut ok = true;
    let unary_loads: Vec<UnaryLoad> = if options.smoke {
        vec![UnaryLoad {
            rate: 500,
            payload: 64,
            concurrency: 256,
        }]
    } else {
        vec![
            UnaryLoad {
                rate: 1_000,
                payload: 64,
                concurrency: 256,
            },
            UnaryLoad {
                rate: 5_000,
                payload: 64,
                concurrency: 256,
            },
            UnaryLoad {
                rate: 20_000,
                payload: 64,
                concurrency: 256,
            },
            UnaryLoad {
                rate: 5_000,
                payload: 4096,
                concurrency: 256,
            },
            UnaryLoad {
                rate: 1_000,
                payload: 65_536,
                concurrency: 256,
            },
            UnaryLoad {
                rate: 5_000,
                payload: 64,
                concurrency: 16,
            },
            UnaryLoad {
                rate: 5_000,
                payload: 64,
                concurrency: 1,
            },
        ]
    };
    println!("\n### Unary, open loop ({flavor}-thread runtime)\n");
    println!(
        "| offered/s | payload | max in flight | completed/s | p50 | p99 | p99.9 | max | failed | shed | peak queued | peak owed | peak heap |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for load in &unary_loads {
        let pair = Pair::start().await?;
        let mut result = unary_load(&pair, load, options.seconds).await;
        let throughput = f64::from(u32::try_from(result.completed).unwrap_or(u32::MAX))
            / result.elapsed.as_secs_f64();
        println!(
            "| {} | {} B | {} | {throughput:.0} | {:.2?} | {:.2?} | {:.2?} | {:.2?} | {} | {} | {} B | {} | {:.1} MiB |",
            load.rate,
            load.payload,
            load.concurrency,
            percentile(&mut result.samples, 500),
            percentile(&mut result.samples, 990),
            percentile(&mut result.samples, 999),
            percentile(&mut result.samples, 1000),
            result.failed,
            result.shed,
            result.peaks.queued_bytes,
            result.peaks.inflight,
            mib(result.peaks.heap),
        );
        ok &= result.failed == 0 && result.completed + result.shed == result.offered;
        ok &= pair.finish().await;
    }
    Ok(ok)
}

async fn soak_streams(options: &Options, flavor: &str) -> Result<bool, Error> {
    let mut ok = true;
    println!("\n### Reply streams, 1 KiB items ({flavor}-thread runtime)\n");
    println!("| streams | window | consumed MiB/s | peak queued | peak heap |");
    println!("|---:|---:|---:|---:|---:|");
    let stream_loads: &[(usize, u64)] = if options.smoke {
        &[(8, 64 << 10)]
    } else {
        &[
            (1, 64 << 10),
            (1, 1 << 20),
            (8, 64 << 10),
            (8, 1 << 20),
            (64, 64 << 10),
            (64, 1 << 20),
        ]
    };
    for &(streams, window) in stream_loads {
        let pair = Pair::start().await?;
        let (rate, peaks) = stream_load(&pair, streams, window, options.seconds).await;
        println!(
            "| {streams} | {} KiB | {:.1} | {} B | {:.1} MiB |",
            window >> 10,
            rate / f64::from(1u32 << 20),
            peaks.queued_bytes,
            mib(peaks.heap)
        );
        ok &= rate > 0.0;
        ok &= pair.finish().await;
    }
    Ok(ok)
}

async fn soak_overload(options: &Options, flavor: &str) -> Result<bool, Error> {
    println!("\n### Overload burst and recovery ({flavor}-thread runtime)\n");
    println!(
        "| burst | served | refused Overloaded | first call served after | peak owed | peak pending | peak tasks | peak heap | final baseline |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---|");
    let burst = if options.smoke { 2_500 } else { 8_192 };
    let pair = Pair::start().await?;
    let (refused, served, recovered, peaks) = overload(&pair, burst).await;
    let baseline = pair.finish().await;
    println!(
        "| {burst} | {served} | {refused} | {recovered:.2?} | {} | {} | {} | {:.1} MiB | {baseline} |",
        peaks.inflight,
        peaks.pending,
        peaks.tasks,
        mib(peaks.heap)
    );
    Ok(refused > 0 && served > 0 && recovered < Duration::from_secs(5) && baseline)
}

fn main() -> Result<(), Error> {
    let options = options();
    println!(
        "# rpc_soak: {} {}, {} CPUs, {} build, {} s per scenario{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        options.seconds,
        if options.smoke { ", smoke" } else { "" }
    );
    let mut ok = true;
    for flavor in &options.flavors {
        let runtime = if *flavor == "current" {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
        } else {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?
        };
        ok &= runtime.block_on(soak(&options, flavor))?;
    }
    if !ok {
        return Err("an invariant failed: see the tables above".into());
    }
    println!("\nevery invariant held");
    Ok(())
}
