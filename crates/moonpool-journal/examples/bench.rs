//! A duration-based write benchmark for the journal on the real filesystem,
//! through `TokioStorageProvider`. It measures; it changes nothing.
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
//! `TokioStorageProvider` runs file I/O on Tokio's blocking pool, so the
//! numbers include that hand-off.
//!
//! Run it in release mode, on the disk under test (the default directory is
//! under `target/`, never `/tmp`, which can be tmpfs):
//!
//! ```text
//! cargo run --release -p moonpool-journal --example bench -- \
//!     --writers 4 --record-size 4096 --batch 1 --fsync ordered --duration 10
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use moonpool_core::{DirectIo, TokioStorageProvider};
use moonpool_journal::{Batch, Durability, Geometry, ID_SIZE, Journal, JournalConfig, JournalId};
use tokio::sync::Barrier;

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
    /// Write a time series (every 500 ms) to this CSV file.
    #[arg(long)]
    csv: Option<PathBuf>,
    /// Keep the journals on disk after the run.
    #[arg(long)]
    keep: bool,
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

impl Args {
    fn config(&self) -> JournalConfig {
        JournalConfig {
            durability: match self.fsync {
                Fsync::Ordered => Durability::Ordered,
                Fsync::Batched => Durability::Batched,
            },
            geometry: Geometry::default(),
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

/// What one writer measured.
struct WriterReport {
    histogram: Histogram<u64>,
    commits: u64,
    elapsed: Duration,
}

fn main() -> Result<()> {
    let args = Arc::new(Args::parse());
    if args.writers == 0 || args.batch == 0 || args.duration == 0 {
        return Err("--writers, --batch and --duration must be at least 1".into());
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.writers)
        .enable_all()
        .build()?;
    let reports = runtime.block_on(run(Arc::clone(&args)))?;

    if !args.keep {
        for index in 0..args.writers {
            remove_if_present(&args.writer_dir(index))?;
        }
    }
    print_report(&args, fs_type.as_deref(), &reports)
}

async fn run(args: Arc<Args>) -> Result<Vec<WriterReport>> {
    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));
    // The writers and this task: everyone starts once every journal exists.
    let start = Arc::new(Barrier::new(args.writers + 1));
    let mut writers = Vec::with_capacity(args.writers);
    for index in 0..args.writers {
        writers.push(tokio::spawn(writer(
            Arc::clone(&args),
            index,
            Arc::clone(&start),
            Arc::clone(&stop),
            Arc::clone(&counters),
        )));
    }
    start.wait().await;
    let began = Instant::now();
    let sampler = args.csv.clone().map(|path| {
        tokio::spawn(sample(
            path,
            began,
            Arc::clone(&stop),
            Arc::clone(&counters),
        ))
    });
    tokio::time::sleep(Duration::from_secs(args.duration)).await;
    stop.store(true, Ordering::Relaxed);

    let mut reports = Vec::with_capacity(args.writers);
    for writer in writers {
        reports.push(writer.await??);
    }
    if let Some(sampler) = sampler {
        sampler.await??;
    }
    Ok(reports)
}

async fn writer(
    args: Arc<Args>,
    index: usize,
    start: Arc<Barrier>,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) -> Result<WriterReport> {
    let dir = args.writer_dir(index);
    let dir = dir.to_str().ok_or("path is not UTF-8")?;
    let id = JournalId(u128::try_from(index)? + 1);
    let mut journal = Journal::create(
        TokioStorageProvider::new(),
        dir,
        id,
        args.config(),
        b"bench",
    )
    .await?;
    let payload = vec![0xA5_u8; args.record_size];
    let batch_len = u64::try_from(args.batch)?;
    let batch_bytes = batch_len * u64::try_from(args.record_size)?;
    let mut histogram = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
    let mut position = 0_u64;
    let mut commits = 0_u64;

    start.wait().await;
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

        let committing = Instant::now();
        journal.commit(batch).await?;
        let latency = u64::try_from(committing.elapsed().as_micros())?;
        histogram.saturating_record(latency.max(1));

        commits += 1;
        counters.commits.fetch_add(1, Ordering::Relaxed);
        counters.bytes.fetch_add(batch_bytes, Ordering::Relaxed);
    }
    Ok(WriterReport {
        histogram,
        commits,
        elapsed: began.elapsed(),
    })
}

/// Every [`SAMPLE_EVERY`] until `stop`: one CSV row with the rates since the
/// last row and the page-cache dirty bytes, written out at the end.
async fn sample(
    path: PathBuf,
    began: Instant,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) -> Result<()> {
    let mut csv =
        String::from("elapsed_ms,commits_per_s,bytes_per_s,total_commits,total_bytes,dirty_kb\n");
    let mut ticks = tokio::time::interval(SAMPLE_EVERY);
    ticks.tick().await;
    let (mut last_at, mut last_commits, mut last_bytes) = (began, 0, 0);
    while !stop.load(Ordering::Relaxed) {
        ticks.tick().await;
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

fn print_report(args: &Args, fs_type: Option<&str>, reports: &[WriterReport]) -> Result<()> {
    let mut merged = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
    let mut rates = Vec::with_capacity(reports.len());
    let mut commits = 0_u64;
    let mut longest = Duration::ZERO;
    for report in reports {
        merged.add(&report.histogram)?;
        rates.push(per_second(report.commits, report.elapsed.as_micros()));
        commits += report.commits;
        longest = longest.max(report.elapsed);
    }
    let span = longest.as_micros();
    let batch = u64::try_from(args.batch).unwrap_or(u64::MAX);
    let record = u64::try_from(args.record_size).unwrap_or(u64::MAX);
    let bytes = commits.saturating_mul(batch).saturating_mul(record);

    println!(
        "moonpool-journal bench: writers={} record-size={}B batch={} fsync={:?} direct-io={:?} truncate-every={} duration={}s",
        args.writers,
        args.record_size,
        args.batch,
        args.fsync,
        args.direct_io,
        args.truncate_every,
        args.duration,
    );
    println!(
        "path: {} ({})",
        args.path.display(),
        fs_type.unwrap_or("unknown filesystem")
    );
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
