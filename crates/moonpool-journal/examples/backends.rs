//! The same journal on Tokio and on compio, on the real filesystem: which
//! backend commits faster, at what latency, and how fast each recovers.
//!
//! One binary runs every workload on both backends with the same journal
//! code, the same configuration and the same payloads, so the only
//! difference is the provider underneath:
//!
//! - **Tokio**: `TokioStorageProvider` on a multi-thread runtime. File I/O
//!   runs on Tokio's blocking pool; a durable write is one
//!   `pwritev2(RWF_DSYNC)`.
//! - **compio**: `CompioStorageProvider` on one compio runtime (one thread).
//!   File I/O is submitted to `io_uring` when the kernel offers it, else
//!   compio falls back to its polling driver; a durable write is a write
//!   and an `fdatasync`.
//!
//! Workloads:
//!
//! - `sequential`: one journal, one durable commit of one record at a time,
//!   for each `--sizes` entry.
//! - `concurrent`: `--writers` journals committing at once (a journal has
//!   one writer, so N journals is N appends in flight), records of
//!   `--concurrent-size` bytes.
//! - `recovery`: a journal of `--recovery-mib` MiB of 4 KiB records, built
//!   afresh for every run, written back with `sync`, then opened (recovery
//!   scans every record) and replayed whole, with the page cache dropped
//!   first when the process may do so.
//!
//! Each runs buffered and with `DirectIo::Required`; a filesystem without
//! direct I/O reports it unavailable. Every commit syncs.
//! The report is Markdown, printed to stdout. Run it in release mode, on
//! the disk under test:
//!
//! ```text
//! cargo run --release -p moonpool-journal --features compio-providers \
//!     --example backends -- --duration 5
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use hdrhistogram::Histogram;
use moonpool_core::{CompioStorageProvider, DirectIo, LocalStorageProvider, TokioStorageProvider};
use moonpool_journal::{Batch, Durability, ID_SIZE, Journal, JournalConfig, JournalId, OpenError};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

/// The largest commit latency the histograms keep, in microseconds.
const MAX_LATENCY_US: u64 = 3_600_000_000;

/// The record size of the recovery journal.
const RECOVERY_RECORD: usize = 4096;

/// Records per commit while the recovery journal is built.
const RECOVERY_BATCH: usize = 64;

#[derive(Parser, Debug)]
#[command(about = "Compare moonpool-journal on Tokio and compio")]
struct Args {
    /// Directory for the journals; it must be on the disk under test.
    #[arg(long, default_value = "target/journal-backends")]
    path: PathBuf,
    /// Seconds per measured run.
    #[arg(long, default_value_t = 5)]
    duration: u64,
    /// Which backends to run.
    #[arg(long, value_enum, value_delimiter = ',', default_values_t = [Backend::Tokio, Backend::Compio])]
    backends: Vec<Backend>,
    /// Which workloads to run.
    #[arg(long, value_enum, value_delimiter = ',', default_values_t = [Workload::Sequential, Workload::Concurrent, Workload::Recovery])]
    workloads: Vec<Workload>,
    /// Record sizes of the sequential workload, in bytes.
    #[arg(long, value_delimiter = ',', default_values_t = [128, 4096, 65536])]
    sizes: Vec<usize>,
    /// Writer counts of the concurrent workload.
    #[arg(long, value_delimiter = ',', default_values_t = [4, 16])]
    writers: Vec<usize>,
    /// Record size of the concurrent workload, in bytes.
    #[arg(long, default_value_t = 4096)]
    concurrent_size: usize,
    /// The commit protocols to run: `batched` syncs once per commit,
    /// `ordered` twice.
    #[arg(long, value_enum, value_delimiter = ',', default_values_t = [Fsync::Batched, Fsync::Ordered])]
    fsync: Vec<Fsync>,
    /// Payload MiB of the recovery journal.
    #[arg(long, default_value_t = 256)]
    recovery_mib: usize,
    /// Opens of the recovery journal per backend.
    #[arg(long, default_value_t = 3)]
    recovery_runs: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Workload {
    Sequential,
    Concurrent,
    Recovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Fsync {
    Batched,
    Ordered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Backend {
    Tokio,
    Compio,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Tokio => "tokio",
            Self::Compio => "compio",
        }
    }
}

/// Buffered or `DirectIo::Required`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Io {
    Buffered,
    Direct,
}

impl Io {
    const ALL: [Self; 2] = [Self::Buffered, Self::Direct];

    fn name(self) -> &'static str {
        match self {
            Self::Buffered => "buffered",
            Self::Direct => "direct",
        }
    }
}

/// One measured append run.
#[derive(Clone, Copy)]
struct Case {
    backend: Backend,
    io: Io,
    fsync: Fsync,
    writers: usize,
    record: usize,
    duration: Duration,
}

impl Case {
    fn config(self) -> JournalConfig {
        config(self.io, self.fsync)
    }
}

fn config(io: Io, fsync: Fsync) -> JournalConfig {
    JournalConfig {
        durability: match fsync {
            Fsync::Batched => Durability::Batched,
            Fsync::Ordered => Durability::Ordered,
        },
        direct_io: match io {
            Io::Buffered => DirectIo::Disabled,
            Io::Direct => DirectIo::Required,
        },
        ..JournalConfig::default()
    }
}

/// What one writer measured.
struct Writer {
    histogram: Histogram<u64>,
    appends: u64,
}

/// What one append run measured, or why it could not run.
enum Outcome {
    Ran {
        histogram: Histogram<u64>,
        appends: u64,
        elapsed: Duration,
    },
    Unavailable(String),
}

/// The backends in the order case `index` runs them: rotated, so none
/// always runs on the disk another just left behind.
fn order(index: usize, backends: &[Backend]) -> Vec<Backend> {
    let mut order = backends.to_vec();
    if !order.is_empty() {
        let by = index % order.len();
        order.rotate_left(by);
    }
    order
}

fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.path)?;
    let mut report = String::new();
    environment(&args.path, &mut report)?;
    let duration = Duration::from_secs(args.duration);
    let mut index = 0;

    if args.workloads.contains(&Workload::Sequential) {
        writeln!(report, "\n## Sequential durable appends\n")?;
        table_header(&mut report)?;
        for &fsync in &args.fsync {
            for &record in &args.sizes {
                for io in Io::ALL {
                    index += 1;
                    for backend in order(index, &args.backends) {
                        let case = Case {
                            backend,
                            io,
                            fsync,
                            writers: 1,
                            record,
                            duration,
                        };
                        let outcome = appends(&args.path, case)?;
                        row(&mut report, case, &outcome)?;
                        report_flush(&mut report);
                    }
                }
            }
        }
    }
    if args.workloads.contains(&Workload::Concurrent) {
        writeln!(report, "\n## Concurrent writers\n")?;
        table_header(&mut report)?;
        for &fsync in &args.fsync {
            for &writers in &args.writers {
                for io in Io::ALL {
                    index += 1;
                    for backend in order(index, &args.backends) {
                        let case = Case {
                            backend,
                            io,
                            fsync,
                            writers,
                            record: args.concurrent_size,
                            duration,
                        };
                        let outcome = appends(&args.path, case)?;
                        row(&mut report, case, &outcome)?;
                        report_flush(&mut report);
                    }
                }
            }
        }
    }
    if args.workloads.contains(&Workload::Recovery) {
        recovery(&args, &mut report)?;
    }
    report_flush(&mut report);
    remove_if_present(&args.path)?;
    Ok(())
}

/// Print what the report holds so far: it is printed as it grows.
fn report_flush(report: &mut String) {
    print!("{report}");
    report.clear();
}

// ---- appends ----

fn appends(root: &Path, case: Case) -> Result<Outcome> {
    let dir = root.join(format!(
        "{}-{}-{:?}-{}x{}",
        case.backend.name(),
        case.io.name(),
        case.fsync,
        case.writers,
        case.record
    ));
    remove_if_present(&dir)?;
    let dirs = (0..case.writers)
        .map(|index| {
            dir.join(format!("writer-{index}"))
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| Error::from("path is not UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    let outcome = match case.backend {
        Backend::Tokio => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(case.writers.clamp(1, cores()))
                .enable_all()
                .build()?;
            runtime.block_on(tokio_appends(case, dirs))
        }
        Backend::Compio => compio::runtime::Runtime::new()?.block_on(compio_appends(case, dirs)),
    };
    remove_if_present(&dir)?;
    outcome
}

async fn tokio_appends(case: Case, dirs: Vec<String>) -> Result<Outcome> {
    let mut journals = Vec::with_capacity(dirs.len());
    for (index, dir) in dirs.iter().enumerate() {
        let id = JournalId(u128::try_from(index)? + 1);
        match Journal::create(
            TokioStorageProvider::new(),
            dir,
            id,
            case.config(),
            b"bench",
        )
        .await
        {
            Ok(mut journal) => {
                warm_up(&mut journal).await?;
                journals.push(journal);
            }
            Err(error) => return unavailable(case, &error),
        }
    }
    let began = Instant::now();
    let deadline = began + case.duration;
    let tasks: Vec<_> = journals
        .into_iter()
        .map(|journal| tokio::spawn(writer(journal, case.record, deadline)))
        .collect();
    let mut writers = Vec::with_capacity(tasks.len());
    for task in tasks {
        writers.push(task.await??);
    }
    finish(writers, began)
}

async fn compio_appends(case: Case, dirs: Vec<String>) -> Result<Outcome> {
    let mut journals = Vec::with_capacity(dirs.len());
    for (index, dir) in dirs.iter().enumerate() {
        let id = JournalId(u128::try_from(index)? + 1);
        match Journal::create(
            CompioStorageProvider::new(),
            dir,
            id,
            case.config(),
            b"bench",
        )
        .await
        {
            Ok(mut journal) => {
                warm_up(&mut journal).await?;
                journals.push(journal);
            }
            Err(error) => return unavailable(case, &error),
        }
    }
    let began = Instant::now();
    let deadline = began + case.duration;
    let tasks: Vec<_> = journals
        .into_iter()
        .map(|journal| compio::runtime::spawn(writer(journal, case.record, deadline)))
        .collect();
    let mut writers = Vec::with_capacity(tasks.len());
    for task in tasks {
        writers.push(
            task.await
                .map_err(|error| format!("writer task: {error:?}"))??,
        );
    }
    finish(writers, began)
}

/// A journal that `DirectIo::Required` cannot create is reported, not
/// fatal; any other failure is.
fn unavailable(case: Case, error: &OpenError) -> Result<Outcome> {
    if case.io == Io::Direct {
        Ok(Outcome::Unavailable(error.to_string()))
    } else {
        Err(format!("creating a {} journal: {error}", case.backend.name()).into())
    }
}

/// Before the clock starts: create the first segment and commit onto it,
/// so no run times the first roll.
async fn warm_up<P: LocalStorageProvider>(journal: &mut Journal<P>) -> Result<()> {
    let mut batch = Batch::new();
    // The run's first commit overwrites it.
    batch.put(0, [0; ID_SIZE], vec![0; 1]);
    journal.commit(batch).await?;
    Ok(())
}

/// Commit one record at a time until `deadline`, timing each commit.
async fn writer<P: LocalStorageProvider>(
    mut journal: Journal<P>,
    record: usize,
    deadline: Instant,
) -> Result<Writer> {
    let payload = vec![0xA5_u8; record];
    let mut histogram = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
    let mut position = 0_u64;
    while Instant::now() < deadline {
        let mut batch = Batch::new();
        batch.put(position, [0; ID_SIZE], payload.clone());
        position += 1;
        let committing = Instant::now();
        journal.commit(batch).await?;
        histogram.saturating_record(u64::try_from(committing.elapsed().as_micros())?.max(1));
    }
    Ok(Writer {
        histogram,
        appends: position,
    })
}

fn finish(writers: Vec<Writer>, began: Instant) -> Result<Outcome> {
    let elapsed = began.elapsed();
    let mut histogram = Histogram::<u64>::new_with_bounds(1, MAX_LATENCY_US, 3)?;
    let mut appends = 0;
    for writer in writers {
        histogram.add(&writer.histogram)?;
        appends += writer.appends;
    }
    Ok(Outcome::Ran {
        histogram,
        appends,
        elapsed,
    })
}

fn table_header(report: &mut String) -> Result<()> {
    writeln!(
        report,
        "| backend | I/O | fsync | writers | record | appends/s | MiB/s | p50 µs | p99 µs | p99.9 µs |"
    )?;
    writeln!(report, "|---|---|---|---:|---:|---:|---:|---:|---:|---:|")?;
    Ok(())
}

fn row(report: &mut String, case: Case, outcome: &Outcome) -> Result<()> {
    let lead = format!(
        "| {} | {} | {:?} | {} | {} |",
        case.backend.name(),
        case.io.name(),
        case.fsync,
        case.writers,
        bytes(case.record)
    );
    match outcome {
        Outcome::Unavailable(why) => {
            writeln!(report, "{lead} unavailable: {why} | | | | |")?;
        }
        Outcome::Ran {
            histogram,
            appends,
            elapsed,
        } => {
            let span = elapsed.as_micros();
            let payload = appends.saturating_mul(u64::try_from(case.record)?);
            writeln!(
                report,
                "{lead} {} | {} | {} | {} | {} |",
                per_second(*appends, span),
                mebibytes(per_second(payload, span)),
                histogram.value_at_quantile(0.50),
                histogram.value_at_quantile(0.99),
                histogram.value_at_quantile(0.999),
            )?;
        }
    }
    Ok(())
}

// ---- recovery ----

fn recovery(args: &Args, report: &mut String) -> Result<()> {
    writeln!(report, "\n## Recovery\n")?;
    let cold = drop_page_cache();
    writeln!(
        report,
        "A journal of {} MiB in {} records, opened (recovery reads and checks every record) and then replayed whole. Every run builds a fresh journal with Tokio, writes back every dirty page (`sync`), then {} before it times the open. Each cell is the median of {} runs, and the slowest run.\n",
        args.recovery_mib,
        bytes(RECOVERY_RECORD),
        if cold {
            "drops the page cache (cold)"
        } else {
            "cannot drop the page cache here (warm)"
        },
        args.recovery_runs,
    )?;
    writeln!(
        report,
        "| backend | I/O | entries | open ms (median / max) | replay ms (median / max) | replay MiB/s |"
    )?;
    writeln!(report, "|---|---|---:|---:|---:|---:|")?;
    for (index, io) in Io::ALL.into_iter().enumerate() {
        let dir = args.path.join(format!("recovery-{}", io.name()));
        let dir = dir.to_str().ok_or("path is not UTF-8")?.to_string();
        let config = config(io, Fsync::Batched);
        for backend in order(index, &args.backends) {
            let mut opens = Vec::new();
            let mut replays = Vec::new();
            let mut entries = 0;
            for _ in 0..args.recovery_runs {
                remove_if_present(Path::new(&dir))?;
                let built = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?
                    .block_on(build(dir.clone(), config, args.recovery_mib));
                if let Err(error) = built {
                    if io == Io::Direct {
                        writeln!(
                            report,
                            "| {} | {} | unavailable: {error} | | | |",
                            backend.name(),
                            io.name()
                        )?;
                        break;
                    }
                    return Err(error);
                }
                std::process::Command::new("sync").status()?;
                drop_page_cache();
                let (open, replay, count) = match backend {
                    Backend::Tokio => tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(reopen(TokioStorageProvider::new(), dir.clone(), config))?,
                    Backend::Compio => compio::runtime::Runtime::new()?.block_on(reopen(
                        CompioStorageProvider::new(),
                        dir.clone(),
                        config,
                    ))?,
                };
                opens.push(open);
                replays.push(replay);
                entries = count;
            }
            if opens.is_empty() {
                continue;
            }
            let payload = u64::try_from(entries * RECOVERY_RECORD)?;
            let replay_median = median(&mut replays);
            writeln!(
                report,
                "| {} | {} | {entries} | {} / {} | {} / {} | {} |",
                backend.name(),
                io.name(),
                median(&mut opens).as_millis(),
                opens.iter().max().copied().unwrap_or_default().as_millis(),
                replay_median.as_millis(),
                replays
                    .iter()
                    .max()
                    .copied()
                    .unwrap_or_default()
                    .as_millis(),
                mebibytes(per_second(payload, replay_median.as_micros())),
            )?;
        }
        remove_if_present(Path::new(&dir))?;
    }
    Ok(())
}

/// Write `mib` MiB of records into a fresh journal, with Tokio.
async fn build(dir: String, config: JournalConfig, mib: usize) -> Result<()> {
    let mut journal = Journal::create(
        TokioStorageProvider::new(),
        &dir,
        JournalId(7),
        config,
        b"recovery",
    )
    .await?;
    let records = u64::try_from(mib * 1024 * 1024 / RECOVERY_RECORD)?;
    let payload = vec![0x5A_u8; RECOVERY_RECORD];
    let batch_len = u64::try_from(RECOVERY_BATCH)?;
    let mut position = 0;
    while position < records {
        let mut batch = Batch::new();
        for offset in 0..batch_len.min(records - position) {
            batch.put(position + offset, [1; ID_SIZE], payload.clone());
        }
        position += batch_len;
        journal.commit(batch).await?;
    }
    Ok(())
}

/// Open the journal in `dir` and replay all of it: the two durations and
/// the entry count.
async fn reopen<P: LocalStorageProvider>(
    provider: P,
    dir: String,
    config: JournalConfig,
) -> Result<(Duration, Duration, usize)> {
    let opening = Instant::now();
    let (journal, recovery) = Journal::open(provider, &dir, JournalId(7), config)
        .await?
        .ok_or("the recovery journal is gone")?;
    let open = opening.elapsed();
    if !recovery.corrupt.is_empty() {
        return Err(format!("recovery found damage: {recovery:?}").into());
    }
    let replaying = Instant::now();
    let entries = journal.replay(..).await?;
    let replay = replaying.elapsed();
    for (_, entry) in &entries {
        if let Err(error) = entry {
            return Err(format!("replay read a damaged entry: {error}").into());
        }
    }
    Ok((open, replay, entries.len()))
}

// ---- the environment ----

fn environment(path: &Path, report: &mut String) -> Result<()> {
    let path = path.canonicalize()?;
    let mount = mount_of(&path);
    let driver = compio::runtime::Runtime::new()?.driver_type();
    writeln!(report, "# moonpool-journal on Tokio and compio\n")?;
    writeln!(report, "| | |\n|---|---|")?;
    writeln!(
        report,
        "| kernel | {} |",
        read_trimmed("/proc/sys/kernel/osrelease")
    )?;
    writeln!(report, "| CPU | {} |", cpu_model())?;
    writeln!(report, "| cores | {} |", cores())?;
    match &mount {
        Some((device, mount_point, kind)) => {
            writeln!(report, "| filesystem | {kind} at {mount_point} |")?;
            writeln!(report, "| device | {device} ({}) |", device_kind(device))?;
        }
        None => writeln!(report, "| filesystem | unknown |")?,
    }
    writeln!(report, "| directory | {} |", path.display())?;
    writeln!(report, "| compio driver | {driver:?} |")?;
    Ok(())
}

fn read_trimmed(path: &str) -> String {
    std::fs::read_to_string(path).map_or_else(|_| "unknown".to_string(), |s| s.trim().to_string())
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, model)| model.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// The device, mount point and filesystem type of the mount holding `path`.
fn mount_of(path: &Path) -> Option<(String, String, String)> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    let mut best: Option<(usize, (String, String, String))> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(device), Some(mount), Some(kind)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let len = Path::new(mount).components().count();
        if path.starts_with(mount) && best.as_ref().is_none_or(|(longest, _)| len >= *longest) {
            best = Some((
                len,
                (device.to_string(), mount.to_string(), kind.to_string()),
            ));
        }
    }
    best.map(|(_, found)| found)
}

/// What `/sys/class/block` says about `device`: rotational or not, and the
/// model when the driver names one.
fn device_kind(device: &str) -> String {
    let Some(name) = device.strip_prefix("/dev/") else {
        return "not a block device".to_string();
    };
    let sys = format!("/sys/class/block/{name}");
    let rotational = match read_trimmed(&format!("{sys}/queue/rotational")).as_str() {
        "0" => "non-rotational",
        "1" => "rotational",
        _ => "rotational unknown",
    };
    let model = read_trimmed(&format!("{sys}/device/model"));
    let vendor = read_trimmed(&format!("{sys}/device/vendor"));
    format!("{rotational}, model {model}, vendor {vendor}")
}

/// Drop the clean pages of the page cache: whether this process may.
fn drop_page_cache() -> bool {
    // The journal synced its own files; a dirty page is never dropped.
    std::fs::write("/proc/sys/vm/drop_caches", "1").is_ok()
}

// ---- arithmetic ----

fn median(values: &mut [Duration]) -> Duration {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or_default()
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

/// A record size, as `128 B`, `4 KiB` or `64 KiB`.
fn bytes(len: usize) -> String {
    if len >= 1024 && len.is_multiple_of(1024) {
        format!("{} KiB", len / 1024)
    } else {
        format!("{len} B")
    }
}

fn remove_if_present(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}
