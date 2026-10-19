//! A duration-based write benchmark for the journal on the real filesystem,
//! on tokio or on compio. It measures; it changes nothing.
//!
//! Each writer task owns one journal in its own directory, as one acceptor
//! or one tenant would: `Journal::commit` takes `&mut self`, so a journal has
//! one writer and there is no group commit to share. The writers start
//! together, commit back to back for `--duration` seconds, and time only
//! `commit` (the batch is built before the clock starts).
//!
//! The report gives commits/s, records/s and MiB/s of payload, the commit
//! latency percentiles from per-writer HDR histograms merged at the end, and
//! the fairness between writers (100% minus the standard deviation of the
//! per-writer commit rates over their median). `--csv` writes a time series
//! every 500 ms with the page-cache dirty bytes from `/proc/meminfo`.
//!
//! Every commit syncs: `--fsync ordered` is two syncs per commit,
//! `--fsync batched` one. Never compare numbers across fsync policies.
//!
//! # Runtimes
//!
//! The measurement is generic over the provider bundle, so `--runtime`
//! switches the storage backend and nothing else; compare runs with
//! identical flags.
//!
//! - `--runtime tokio` (the default) runs on `TokioProviders` and a
//!   multi-thread tokio runtime with one worker thread per writer, or
//!   `--threads N`. `TokioStorageProvider` runs file I/O on Tokio's blocking
//!   pool, so the numbers include that hand-off.
//! - `--runtime compio` runs on `CompioProviders`: positioned I/O is
//!   `io_uring` (epoll when the kernel refuses `io_uring`; the report names the
//!   driver), durable writes are a write plus `sync_data` (compio has no
//!   per-write `RWF_DSYNC`), and there is no blocking-pool hand-off. With
//!   `--threads 1` (the default) everything runs on one compio runtime. With
//!   `--threads N` the writers are spread over N threads, one compio runtime
//!   each (thread-per-core), and the main thread's own runtime keeps the
//!   clock and the `--csv` sampler.
//!
//! Run it in release mode, on the disk under test (the default directory is
//! under `target/`, never `/tmp`, which can be tmpfs):
//!
//! ```text
//! cargo run --release -p moonpool-journal-bench -- \
//!     --writers 4 --record-size 4096 --batch 1 --fsync ordered --duration 10
//! cargo run --release -p moonpool-journal-bench -- --runtime compio --threads 4 \
//!     --writers 4 --record-size 4096 --batch 1 --fsync ordered --duration 10
//! ```

use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use moonpool_compio::CompioProviders;
use moonpool_core::{Detach, DirectIo, Providers, TaskProvider, TimeProvider, TokioProviders};
use moonpool_journal::{Batch, Durability, Geometry, ID_SIZE, Journal, JournalConfig, JournalId};
use tokio::sync::{Barrier, oneshot};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

/// The largest commit latency the histograms keep, in microseconds (one hour).
const MAX_LATENCY_US: u64 = 3_600_000_000;

/// How often the `--csv` sampler records a row.
const SAMPLE_EVERY: Duration = Duration::from_millis(500);

#[derive(Parser, Debug)]
#[command(about = "Measure moonpool-journal commits on the real filesystem")]
struct Args {
    /// Directory for the journals (one subdirectory per writer). It must be
    /// on the disk under test; tmpfs and ramfs are refused.
    #[arg(long, default_value = "target/journal-bench")]
    path: PathBuf,
    /// Seconds of measurement.
    #[arg(long, default_value_t = 10)]
    duration: u64,
    /// Writer tasks, one journal each.
    #[arg(long, default_value_t = 1)]
    writers: usize,
    /// Payload bytes per record.
    #[arg(long, default_value_t = 1024)]
    record_size: usize,
    /// Records per commit.
    #[arg(long, default_value_t = 1)]
    batch: usize,
    /// The commit protocol: `ordered` syncs twice per commit, `batched` once.
    #[arg(long, value_enum, default_value_t = Fsync::Ordered)]
    fsync: Fsync,
    /// Direct-I/O policy for the segment files.
    #[arg(long, value_enum, default_value_t = Direct::Disabled)]
    direct_io: Direct,
    /// Raise the floor every N commits, so whole segments are deleted while
    /// the run goes on. The floor lands with the metainfo: two more syncs on
    /// that commit. 0 never raises it.
    #[arg(long, default_value_t = 0)]
    truncate_every: u64,
    /// Blocks in each segment's persist log and in its entry log (no gap),
    /// to roll often; 0 keeps the default 64 MiB segment.
    #[arg(long, default_value_t = 0)]
    segment_blocks: u32,
    /// Prepare each next segment in the background (a spare), so a roll is
    /// a header write and a rename; off, a roll zero-fills a whole segment
    /// on the commit path.
    #[arg(long)]
    spares: bool,
    /// Write a time series (every 500 ms) to this CSV file.
    #[arg(long)]
    csv: Option<PathBuf>,
    /// Keep the journals on disk after the run.
    #[arg(long)]
    keep: bool,
    /// The async runtime, and so the storage backend, the journals run on.
    #[arg(long, value_enum, default_value_t = Runtime::Tokio)]
    runtime: Runtime,
    /// OS threads for the writers: tokio's worker threads (default: one per
    /// writer), or compio runtimes, one per thread, sharing the writers
    /// (default: 1, and never more than the writers).
    #[arg(long)]
    threads: Option<usize>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Fsync {
    /// `Durability::Ordered`: two syncs per commit.
    Ordered,
    /// `Durability::Batched`: one sync per commit.
    Batched,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Direct {
    /// Buffered I/O.
    Disabled,
    /// `O_DIRECT` when the filesystem supports it.
    Optional,
    /// `O_DIRECT` or fail.
    Required,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Runtime {
    /// `TokioProviders` on a multi-thread tokio runtime.
    Tokio,
    /// `CompioProviders` on one compio runtime per thread.
    Compio,
}

impl Args {
    fn config(&self) -> JournalConfig {
        JournalConfig {
            durability: match self.fsync {
                Fsync::Ordered => Durability::Ordered,
                Fsync::Batched => Durability::Batched,
            },
            geometry: if self.segment_blocks == 0 {
                Geometry::default()
            } else {
                Geometry {
                    persist_blocks: self.segment_blocks,
                    gap_blocks: 0,
                    entry_blocks: self.segment_blocks,
                }
            },
            direct_io: match self.direct_io {
                Direct::Disabled => DirectIo::Disabled,
                Direct::Optional => DirectIo::Optional,
                Direct::Required => DirectIo::Required,
            },
        }
    }

    fn writer_dir(&self, index: usize) -> PathBuf {
        self.path.join(format!("writer-{index}"))
    }
}

/// Counters every writer adds to after each commit, read by the sampler.
#[derive(Default)]
struct Counters {
    commits: AtomicU64,
    bytes: AtomicU64,
}

/// What the writers and the coordinating task share, across threads.
#[derive(Clone)]
struct Shared {
    start: Arc<Barrier>,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
}

impl Shared {
    fn new(writers: usize) -> Self {
        Self {
            // The writers and the coordinating task: everyone starts once
            // every journal exists.
            start: Arc::new(Barrier::new(writers + 1)),
            stop: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(Counters::default()),
        }
    }
}

/// What one writer measured.
struct WriterReport {
    histogram: Histogram<u64>,
    /// The commits that rolled onto a prepared spare.
    spare_rolls: Histogram<u64>,
    /// The commits that created their new segment inline.
    inline_rolls: Histogram<u64>,
    commits: u64,
    elapsed: Duration,
}

/// A writer's report, sent back from its task (a provider's join handle
/// carries no value).
type ReportReceiver = oneshot::Receiver<Result<WriterReport>>;

fn main() -> Result<()> {
    let args = Arc::new(Args::parse());
    if args.writers == 0 || args.batch == 0 || args.duration == 0 || args.threads == Some(0) {
        return Err("--writers, --batch, --duration and --threads must be at least 1".into());
    }
    if !args.config().geometry.is_valid() {
        return Err("--segment-blocks describes no usable segment".into());
    }
    let mut probe = Batch::new();
    for position in 0..args.batch {
        probe.put(
            u64::try_from(position)?,
            [0; ID_SIZE],
            vec![0; args.record_size],
        );
    }
    if !probe.fits(args.config().geometry) {
        return Err(
            "one commit of --batch records of --record-size bytes does not fit a segment".into(),
        );
    }
    std::fs::create_dir_all(&args.path)?;
    let fs_type = filesystem_type(&args.path)?;
    if matches!(fs_type.as_deref(), Some("tmpfs" | "ramfs")) {
        return Err(format!(
            "{} is on {}: run on a real disk",
            args.path.display(),
            fs_type.unwrap_or_default()
        )
        .into());
    }
    for index in 0..args.writers {
        remove_if_present(&args.writer_dir(index))?;
    }

    let (reports, runtime) = match args.runtime {
        Runtime::Tokio => run_tokio(&args)?,
        Runtime::Compio => run_compio(&args)?,
    };

    if !args.keep {
        for index in 0..args.writers {
            remove_if_present(&args.writer_dir(index))?;
        }
    }
    print_report(&args, fs_type.as_deref(), &runtime, &reports)
}

/// Every writer on one multi-thread tokio runtime. Returns the reports and
/// the runtime line of the report.
fn run_tokio(args: &Arc<Args>) -> Result<(Vec<WriterReport>, String)> {
    let threads = args.threads.unwrap_or(args.writers);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?;
    let reports = runtime.block_on(async { run(&TokioProviders::new(), args).await })?;
    Ok((reports, format!("tokio, {threads} worker threads")))
}

/// The writers on `--threads` compio runtimes, one per thread. Returns the
/// reports and the runtime line of the report.
fn run_compio(args: &Arc<Args>) -> Result<(Vec<WriterReport>, String)> {
    let threads = args.threads.unwrap_or(1).min(args.writers);
    let runtime = compio::runtime::Runtime::new()?;
    let label = format!(
        "compio ({:?} driver), {threads} thread{}",
        runtime.driver_type(),
        if threads == 1 { "" } else { "s" },
    );
    let reports = if threads == 1 {
        runtime.block_on(run(&CompioProviders::new(), args))?
    } else {
        run_compio_threads(&runtime, args, threads)?
    };
    Ok((reports, label))
}

/// Thread-per-core: `threads` threads, each with its own compio runtime and
/// its share of the writers; `runtime` (the main thread's) keeps the clock
/// and the sampler.
fn run_compio_threads(
    runtime: &compio::runtime::Runtime,
    args: &Arc<Args>,
    threads: usize,
) -> Result<Vec<WriterReport>> {
    let shared = Shared::new(args.writers);
    let mut handles = Vec::with_capacity(threads);
    for (thread, indices) in split(args.writers, threads).into_iter().enumerate() {
        let (args, shared) = (Arc::clone(args), shared.clone());
        handles.push(
            std::thread::Builder::new()
                .name(format!("bench-compio-{thread}"))
                .spawn(move || -> Result<Vec<WriterReport>> {
                    compio::runtime::Runtime::new()?.block_on(async {
                        collect(spawn_writers(
                            &CompioProviders::new(),
                            &args,
                            indices,
                            &shared,
                        ))
                        .await
                    })
                })?,
        );
    }
    let coordinated = runtime.block_on(coordinate(&CompioProviders::new(), args, &shared));
    let mut reports = Vec::with_capacity(args.writers);
    for handle in handles {
        reports.extend(handle.join().map_err(|_| "a writer thread panicked")??);
    }
    coordinated?;
    Ok(reports)
}

/// The writer indices of each of `threads` threads: contiguous, and the
/// first `writers % threads` threads take one more.
fn split(writers: usize, threads: usize) -> Vec<Range<usize>> {
    let (each, extra) = (writers / threads, writers % threads);
    let mut next = 0;
    (0..threads)
        .map(|thread| {
            let len = each + usize::from(thread < extra);
            let indices = next..next + len;
            next += len;
            indices
        })
        .collect()
}

/// Every writer and the clock on the runtime `providers` runs on.
async fn run<P: Providers>(providers: &P, args: &Arc<Args>) -> Result<Vec<WriterReport>> {
    let shared = Shared::new(args.writers);
    let writers = spawn_writers(providers, args, 0..args.writers, &shared);
    coordinate(providers, args, &shared).await?;
    collect(writers).await
}

/// Spawn the writers `indices` as tasks of `providers`.
fn spawn_writers<P: Providers>(
    providers: &P,
    args: &Arc<Args>,
    indices: Range<usize>,
    shared: &Shared,
) -> Vec<ReportReceiver> {
    indices
        .map(|index| {
            let (sender, receiver) = oneshot::channel();
            let report = writer(providers.clone(), Arc::clone(args), index, shared.clone());
            providers
                .task()
                .spawn_task(&format!("writer-{index}"), async move {
                    // The receiver is gone only when the run already failed.
                    let _ = sender.send(report.await);
                })
                .detach();
            receiver
        })
        .collect()
}

/// Wait for every writer's report.
async fn collect(receivers: Vec<ReportReceiver>) -> Result<Vec<WriterReport>> {
    let mut reports = Vec::with_capacity(receivers.len());
    for receiver in receivers {
        reports.push(
            receiver
                .await
                .map_err(|_| "a writer task ended without a report")??,
        );
    }
    Ok(reports)
}

/// Start the writers together, run the sampler, and stop the writers after
/// `--duration`.
async fn coordinate<P: Providers>(providers: &P, args: &Args, shared: &Shared) -> Result<()> {
    shared.start.wait().await;
    let began = Instant::now();
    let sampler = args.csv.clone().map(|path| {
        let (sender, receiver) = oneshot::channel();
        let sample = sample(
            providers.time().clone(),
            path,
            began,
            Arc::clone(&shared.stop),
            Arc::clone(&shared.counters),
        );
        providers
            .task()
            .spawn_task("sampler", async move {
                // The receiver is gone only when the run already failed.
                let _ = sender.send(sample.await);
            })
            .detach();
        receiver
    });
    let slept = providers
        .time()
        .sleep(Duration::from_secs(args.duration))
        .await;
    // Stop the writers even when the clock failed, so nothing waits forever.
    shared.stop.store(true, Ordering::Relaxed);
    slept?;
    if let Some(sampler) = sampler {
        sampler
            .await
            .map_err(|_| "the sampler ended without a result")??;
    }
    Ok(())
}

async fn writer<P: Providers>(
    providers: P,
    args: Arc<Args>,
    index: usize,
    shared: Shared,
) -> Result<WriterReport> {
    let Shared {
        start,
        stop,
        counters,
    } = shared;
    // Setup errors wait for the barrier too, so one failed writer cannot
    // leave the others (and the clock) waiting for it forever.
    let setup = async {
        let dir = args.writer_dir(index);
        let dir = dir.to_str().ok_or("path is not UTF-8")?;
        let id = JournalId(u128::try_from(index)? + 1);
        let mut journal = Journal::create(
            providers.storage().clone(),
            dir,
            id,
            args.config(),
            b"bench",
        )
        .await?;
        if args.spares {
            journal.prepare_spares_with(providers.task().clone());
        }
        let batch_len = u64::try_from(args.batch)?;
        let batch_bytes = batch_len * u64::try_from(args.record_size)?;
        let histogram = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
        let spare_rolls = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
        let inline_rolls = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
        Ok::<_, Error>((
            journal,
            batch_len,
            batch_bytes,
            histogram,
            spare_rolls,
            inline_rolls,
        ))
    }
    .await;
    start.wait().await;
    let (mut journal, batch_len, batch_bytes, mut histogram, mut spare_rolls, mut inline_rolls) =
        setup?;
    let payload = vec![0xA5_u8; args.record_size];
    let mut position = 0_u64;
    let mut commits = 0_u64;

    let began = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut batch = Batch::new();
        for offset in 0..batch_len {
            batch.put(position + offset, [0; ID_SIZE], payload.clone());
        }
        if args.truncate_every > 0 && (commits + 1).is_multiple_of(args.truncate_every) {
            batch.truncate_prefix(position);
        }
        position += batch_len;

        let rolled = journal.rolls();
        let committing = Instant::now();
        journal.commit(batch).await?;
        let latency = u64::try_from(committing.elapsed().as_micros())?;
        histogram.saturating_record(latency.max(1));
        let now = journal.rolls();
        if now.onto_spare != rolled.onto_spare {
            spare_rolls.saturating_record(latency.max(1));
        }
        if now.inline != rolled.inline {
            inline_rolls.saturating_record(latency.max(1));
        }

        commits += 1;
        counters.commits.fetch_add(1, Ordering::Relaxed);
        counters.bytes.fetch_add(batch_bytes, Ordering::Relaxed);
    }
    Ok(WriterReport {
        histogram,
        spare_rolls,
        inline_rolls,
        commits,
        elapsed: began.elapsed(),
    })
}

/// Every [`SAMPLE_EVERY`] until `stop`: one CSV row with the rates since the
/// last row and the page-cache dirty bytes, written out at the end.
async fn sample<T: TimeProvider>(
    time: T,
    path: PathBuf,
    began: Instant,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) -> Result<()> {
    let mut csv =
        String::from("elapsed_ms,commits_per_s,bytes_per_s,total_commits,total_bytes,dirty_kb\n");
    let (mut last_at, mut last_commits, mut last_bytes) = (began, 0, 0);
    while !stop.load(Ordering::Relaxed) {
        time.sleep(SAMPLE_EVERY).await?;
        let now = Instant::now();
        let commits = counters.commits.load(Ordering::Relaxed);
        let bytes = counters.bytes.load(Ordering::Relaxed);
        let span = now.duration_since(last_at).as_micros();
        writeln!(
            csv,
            "{},{},{},{commits},{bytes},{}",
            now.duration_since(began).as_millis(),
            per_second(commits - last_commits, span),
            per_second(bytes - last_bytes, span),
            dirty_kb().map_or_else(String::new, |kb| kb.to_string()),
        )?;
        (last_at, last_commits, last_bytes) = (now, commits, bytes);
    }
    std::fs::write(path, csv)?;
    Ok(())
}

fn print_report(
    args: &Args,
    fs_type: Option<&str>,
    runtime: &str,
    reports: &[WriterReport],
) -> Result<()> {
    let mut merged = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
    let mut spare_rolls = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
    let mut inline_rolls = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
    let mut rates = Vec::with_capacity(reports.len());
    let mut commits = 0_u64;
    let mut longest = Duration::ZERO;
    for report in reports {
        merged.add(&report.histogram)?;
        spare_rolls.add(&report.spare_rolls)?;
        inline_rolls.add(&report.inline_rolls)?;
        rates.push(per_second(report.commits, report.elapsed.as_micros()));
        commits += report.commits;
        longest = longest.max(report.elapsed);
    }
    let span = longest.as_micros();
    let batch = u64::try_from(args.batch).unwrap_or(u64::MAX);
    let record = u64::try_from(args.record_size).unwrap_or(u64::MAX);
    let bytes = commits.saturating_mul(batch).saturating_mul(record);

    println!(
        "moonpool-journal bench: writers={} record-size={}B batch={} fsync={:?} direct-io={:?} truncate-every={} segment-blocks={} spares={} duration={}s",
        args.writers,
        args.record_size,
        args.batch,
        args.fsync,
        args.direct_io,
        args.truncate_every,
        args.segment_blocks,
        args.spares,
        args.duration,
    );
    println!(
        "path: {} ({})",
        args.path.display(),
        fs_type.unwrap_or("unknown filesystem")
    );
    println!("runtime: {runtime}");
    println!(
        "throughput: {} commits/s, {} records/s, {} MiB/s of payload ({commits} commits)",
        per_second(commits, span),
        per_second(commits.saturating_mul(batch), span),
        mebibytes(per_second(bytes, span)),
    );
    println!(
        "commit latency (µs): min {} avg {:.0} p50 {} p90 {} p95 {} p99 {} p99.9 {} max {}",
        merged.min(),
        merged.mean(),
        merged.value_at_quantile(0.50),
        merged.value_at_quantile(0.90),
        merged.value_at_quantile(0.95),
        merged.value_at_quantile(0.99),
        merged.value_at_quantile(0.999),
        merged.max(),
    );
    for (kind, rolls) in [("inline", &inline_rolls), ("onto a spare", &spare_rolls)] {
        if !rolls.is_empty() {
            println!(
                "commits rolling {kind} (µs): {} of them, min {} p50 {} p99 {} max {}",
                rolls.len(),
                rolls.min(),
                rolls.value_at_quantile(0.50),
                rolls.value_at_quantile(0.99),
                rolls.max(),
            );
        }
    }
    println!(
        "fairness: {}% (per-writer commits/s: {rates:?})",
        fairness(&rates)
    );
    Ok(())
}

/// `count` per second over `span_us` microseconds.
fn per_second(count: u64, span_us: u128) -> u64 {
    if span_us == 0 {
        return 0;
    }
    u64::try_from(u128::from(count) * 1_000_000 / span_us).unwrap_or(u64::MAX)
}

/// Bytes as MiB with one decimal.
fn mebibytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    format!("{}.{}", bytes / MIB, bytes % MIB * 10 / MIB)
}

/// 100% minus the standard deviation of the rates over their median.
fn fairness(rates: &[u64]) -> u64 {
    let mut sorted = rates.to_vec();
    sorted.sort_unstable();
    let Some(&median) = sorted.get(sorted.len() / 2) else {
        return 100;
    };
    if median == 0 {
        return 0;
    }
    let n = u128::try_from(rates.len()).unwrap_or(u128::MAX);
    let mean = rates.iter().map(|&r| u128::from(r)).sum::<u128>() / n;
    let variance = rates
        .iter()
        .map(|&r| u128::from(r).abs_diff(mean).pow(2))
        .sum::<u128>()
        / n;
    let spread = variance.isqrt() * 100 / u128::from(median);
    u64::try_from(100_u128.saturating_sub(spread)).unwrap_or(0)
}

/// `Dirty:` from `/proc/meminfo`, in KiB; `None` off Linux.
fn dirty_kb() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("Dirty:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// The filesystem type of the mount holding `path`, from `/proc/mounts`;
/// `None` off Linux.
fn filesystem_type(path: &Path) -> Result<Option<String>> {
    let path = path.canonicalize()?;
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return Ok(None);
    };
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_device), Some(mount), Some(kind)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let len = Path::new(mount).components().count();
        if path.starts_with(mount) && best.as_ref().is_none_or(|(longest, _)| len >= *longest) {
            best = Some((len, kind.to_string()));
        }
    }
    Ok(best.map(|(_, kind)| kind))
}

fn remove_if_present(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}
