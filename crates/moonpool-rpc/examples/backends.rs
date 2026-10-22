//! The same RPC runtime on Tokio and on compio, over real loopback TCP:
//! how many echo calls per second each backend completes, at what latency,
//! and how much process CPU time each call costs.
//!
//! One binary runs the same runtime code, the same configuration and the
//! same payloads on every backend, so the only difference is the provider
//! underneath:
//!
//! - `tokio`: `TokioProviders`, one current-thread runtime for the server
//!   and one for the client, each on its own OS thread. Sockets are driven
//!   by epoll readiness, and every read and write is its own syscall.
//! - `tokio-mt`: `TokioProviders`, one multi-thread runtime (4 workers)
//!   that runs both sides, as a Tokio service does.
//! - `compio`: `CompioProviders`, one compio runtime for each side, each on
//!   its own OS thread. Socket I/O is submitted to `io_uring` when the
//!   kernel offers it; otherwise compio falls back to its polling driver
//!   (a Docker seccomp profile blocks `io_uring` by default).
//!
//! In each run, `--callers` calls are in flight at the same time on one
//! client runtime for `--duration` seconds. Each call sends `--size`
//! bytes, and the reply has the same bytes. The report is Markdown,
//! printed to stdout. Run it in release mode:
//!
//! ```text
//! cargo run --release -p moonpool-rpc --features compio-providers \
//!     --example backends -- --duration 5 --callers 1,32 --size 128
//! ```

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use futures::future::{Either, select};
use moonpool_core::{CompioProviders, SystemProvider, TokioProviders, TokioSystemProvider};
use moonpool_rpc::security::SecurityConfig;
use moonpool_rpc::{
    AccessClass, IncomingRequest, MethodId, RpcConfig, RpcDriver, RpcMethod, RpcProviders,
    SchemaVersion, ServiceRef,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, PartialEq, prost::Message)]
struct Blob {
    #[prost(bytes = "vec", tag = "1")]
    body: Vec<u8>,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Blob;
    type Reply = Blob;
    const METHOD: MethodId = MethodId::new(0xBE7C);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "bench.echo";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Tokio,
    TokioMultiThread,
    Compio,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Tokio => "tokio",
            Self::TokioMultiThread => "tokio-mt",
            Self::Compio => "compio",
        }
    }

    fn parse(name: &str) -> Result<Self, Error> {
        match name {
            "tokio" => Ok(Self::Tokio),
            "tokio-mt" => Ok(Self::TokioMultiThread),
            "compio" => Ok(Self::Compio),
            other => Err(format!("unknown backend `{other}`").into()),
        }
    }
}

struct Args {
    duration: Duration,
    callers: Vec<usize>,
    size: usize,
    backends: Vec<Backend>,
}

fn list<T>(value: &str, parse: impl Fn(&str) -> Result<T, Error>) -> Result<Vec<T>, Error> {
    value.split(',').map(|item| parse(item.trim())).collect()
}

fn parse_args() -> Result<Args, Error> {
    let mut args = Args {
        duration: Duration::from_secs(5),
        callers: vec![1, 32],
        size: 128,
        backends: vec![Backend::Tokio, Backend::TokioMultiThread, Backend::Compio],
    };
    let mut items = std::env::args().skip(1);
    while let Some(flag) = items.next() {
        let value = items
            .next()
            .ok_or_else(|| format!("`{flag}` needs a value"))?;
        match flag.as_str() {
            "--duration" => args.duration = Duration::from_secs_f64(value.parse()?),
            "--callers" => args.callers = list(&value, |item| Ok(item.parse()?))?,
            "--size" => args.size = value.parse()?,
            "--backends" => args.backends = list(&value, Backend::parse)?,
            other => {
                return Err(format!(
                    "unknown flag `{other}` (use --duration, --callers, --size, --backends)"
                )
                .into());
            }
        }
    }
    Ok(args)
}

fn config() -> RpcConfig {
    RpcConfig {
        security: SecurityConfig::trusted_network(),
        ..RpcConfig::default()
    }
}

/// Bind, publish the reference through `reference`, and answer every echo
/// until `stop` resolves. The handler runs next to the driver in one
/// future, so no task is spawned on any backend.
async fn serve<P: RpcProviders>(
    providers: P,
    reference: std::sync::mpsc::Sender<Vec<u8>>,
    stop: futures::channel::oneshot::Receiver<()>,
) {
    let (driver, rpc) = RpcDriver::listen(providers, "127.0.0.1:0", config())
        .await
        .expect("bind an ephemeral port");
    let (service, mut stream) = rpc
        .register::<Echo>(AccessClass::Private)
        .expect("register");
    reference.send(service.to_bytes()).expect("send reference");
    let handler = async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let _ = reply.send(&request);
        }
    };
    let serving = select(Box::pin(driver.run()), Box::pin(handler));
    let _ = select(serving, stop).await;
}

/// What one run measured.
struct Measured {
    calls: u64,
    elapsed: Duration,
    /// Every call's latency, in microseconds.
    latencies: Vec<u64>,
}

/// `callers` callers, each calling in a loop until `duration` has passed.
async fn call<P: RpcProviders>(
    providers: P,
    reference: Vec<u8>,
    callers: usize,
    size: usize,
    duration: Duration,
) -> Measured {
    let (driver, rpc) = RpcDriver::client_only(providers, config()).expect("valid config");
    let echo = ServiceRef::<Echo>::from_bytes(&reference)
        .expect("decodes")
        .bind(&rpc);
    let request = Blob {
        body: vec![0x5A; size],
    };
    let workload = async {
        // One call first, so the dial and the handshake are not measured.
        echo.try_get_reply(&request).await.expect("warm-up reply");
        let start = Instant::now();
        let deadline = start + duration;
        let loops = (0..callers).map(|_| async {
            let mut latencies = Vec::new();
            while Instant::now() < deadline {
                let sent = Instant::now();
                let reply = echo.try_get_reply(&request).await.expect("reply");
                assert_eq!(reply.body.len(), size);
                latencies.push(u64::try_from(sent.elapsed().as_micros()).unwrap_or(u64::MAX));
            }
            latencies
        });
        let latencies: Vec<u64> = futures::future::join_all(loops)
            .await
            .into_iter()
            .flatten()
            .collect();
        Measured {
            calls: latencies.len() as u64,
            elapsed: start.elapsed(),
            latencies,
        }
    };
    match select(Box::pin(driver.run()), Box::pin(workload)).await {
        Either::Left((error, _)) => panic!("client driver stopped: {error}"),
        Either::Right((measured, _)) => measured,
    }
}

/// Run one side on its own OS thread, on a fresh runtime of `backend`.
fn on_thread<T: Send + 'static>(
    backend: Backend,
    side: impl FnOnce(Backend) -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    std::thread::spawn(move || side(backend))
}

fn block_on<F: std::future::Future>(backend: Backend, future: F) -> F::Output {
    match backend {
        Backend::Compio => compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(future),
        Backend::Tokio | Backend::TokioMultiThread => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(future),
    }
}

/// One server thread and one client thread, each with its own runtime.
fn run_two_threads(backend: Backend, callers: usize, size: usize, duration: Duration) -> Measured {
    let (reference_tx, reference_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = futures::channel::oneshot::channel();
    let server = on_thread(backend, move |backend| {
        block_on(backend, async move {
            if backend == Backend::Compio {
                serve(CompioProviders::new(), reference_tx, stop_rx).await;
            } else {
                serve(TokioProviders::new(), reference_tx, stop_rx).await;
            }
        });
    });
    let reference = reference_rx.recv().expect("reference");
    let client = on_thread(backend, move |backend| {
        block_on(backend, async move {
            if backend == Backend::Compio {
                call(CompioProviders::new(), reference, callers, size, duration).await
            } else {
                call(TokioProviders::new(), reference, callers, size, duration).await
            }
        })
    });
    let measured = client.join().expect("client thread");
    let _ = stop_tx.send(());
    server.join().expect("server thread");
    measured
}

/// Both sides on one multi-thread Tokio runtime.
fn run_multi_thread(callers: usize, size: usize, duration: Duration) -> Measured {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        let (reference_tx, reference_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = futures::channel::oneshot::channel();
        let server = tokio::spawn(serve(TokioProviders::new(), reference_tx, stop_rx));
        let reference = tokio::task::spawn_blocking(move || reference_rx.recv())
            .await
            .expect("join")
            .expect("reference");
        let measured = tokio::spawn(call(
            TokioProviders::new(),
            reference,
            callers,
            size,
            duration,
        ))
        .await
        .expect("client task");
        let _ = stop_tx.send(());
        server.await.expect("server task");
        measured
    })
}

fn percentile(sorted: &[u64], quantile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let last = sorted.len() - 1;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a rank in a vector that fits in memory"
    )]
    let rank = ((last as f64) * quantile).round() as usize;
    sorted[rank.min(last)]
}

fn main() -> Result<(), Error> {
    let args = parse_args()?;
    let system = TokioSystemProvider::new();
    let mut report = String::new();
    writeln!(
        report,
        "Echo calls of {} B, {:.1} s per run, on loopback TCP.\n",
        args.size,
        args.duration.as_secs_f64()
    )?;
    writeln!(
        report,
        "| backend | callers | calls/s | p50 µs | p99 µs | p99.9 µs | CPU µs per call |"
    )?;
    writeln!(report, "|---|---|---|---|---|---|---|")?;
    for &callers in &args.callers {
        for &backend in &args.backends {
            let before = system.sample(".")?.process_cpu;
            let mut measured = match backend {
                Backend::TokioMultiThread => run_multi_thread(callers, args.size, args.duration),
                Backend::Tokio | Backend::Compio => {
                    run_two_threads(backend, callers, args.size, args.duration)
                }
            };
            let cpu = system.sample(".")?.process_cpu.saturating_sub(before);
            measured.latencies.sort_unstable();
            #[expect(
                clippy::cast_precision_loss,
                reason = "a call count, shown to one decimal"
            )]
            let rate = measured.calls as f64 / measured.elapsed.as_secs_f64();
            #[expect(
                clippy::cast_precision_loss,
                reason = "a call count, shown to one decimal"
            )]
            let cpu_per_call = cpu.as_secs_f64() * 1e6 / measured.calls.max(1) as f64;
            writeln!(
                report,
                "| {} | {callers} | {rate:.0} | {} | {} | {} | {cpu_per_call:.1} |",
                backend.name(),
                percentile(&measured.latencies, 0.5),
                percentile(&measured.latencies, 0.99),
                percentile(&measured.latencies, 0.999),
            )?;
        }
    }
    print!("{report}");
    Ok(())
}
