//! Production [`SystemProvider`]: `getrusage`, `/proc/stat`, `/proc/diskstats`
//! and Tokio's runtime metrics.

use std::{io, time::Duration};

use super::{DiskCounters, SystemProvider, SystemSample};

/// Production system provider.
///
/// - `process_cpu`: `getrusage(RUSAGE_SELF)`, user plus system (Unix).
/// - `machine_busy` / `machine_total`: the `cpu` line of `/proc/stat` (Linux).
/// - `disk`: the `/proc/diskstats` line of the device that holds `data_dir`,
///   found by the major and minor numbers of its `stat()` (Linux), the
///   method FDB uses. `None` when no line matches, which happens on
///   btrfs, overlayfs and some container setups.
/// - `run_loop_busy`: the busiest Tokio worker's busy time
///   (`RuntimeMetrics::worker_total_busy_duration`), on targets with 64-bit
///   atomics, when called from inside a Tokio runtime. Tokio adds a worker's
///   busy time when the worker parks, so a reader must clamp the ratio.
/// - `cores`: [`std::thread::available_parallelism`], which honours the
///   cgroup CPU quota and the affinity mask.
#[derive(Debug, Clone)]
pub struct TokioSystemProvider {
    start: std::time::Instant,
}

impl TokioSystemProvider {
    /// Create a system provider. Its [`SystemSample::at`] counts from now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for TokioSystemProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemProvider for TokioSystemProvider {
    fn sample(&self, data_dir: &str) -> io::Result<SystemSample> {
        let at = self.start.elapsed();
        let process_cpu = process_cpu()?;
        let (machine_busy, machine_total) = machine_cpu().unwrap_or((Duration::ZERO, at));
        let cores = std::thread::available_parallelism()
            .ok()
            .and_then(|cores| u32::try_from(cores.get()).ok())
            .unwrap_or(1);
        Ok(SystemSample {
            at,
            process_cpu,
            cores,
            machine_busy,
            machine_total,
            run_loop_busy: run_loop_busy(),
            disk: disk_counters(data_dir),
        })
    }
}

#[cfg(unix)]
fn process_cpu() -> io::Result<Duration> {
    // SAFETY: `getrusage` only writes into the zeroed struct we own.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer to a properly sized `rusage`.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(timeval(usage.ru_utime) + timeval(usage.ru_stime))
}

#[cfg(unix)]
fn timeval(value: libc::timeval) -> Duration {
    let secs = u64::try_from(value.tv_sec).unwrap_or(0);
    let micros = u64::try_from(value.tv_usec).unwrap_or(0);
    Duration::from_secs(secs) + Duration::from_micros(micros)
}

#[cfg(not(unix))]
fn process_cpu() -> io::Result<Duration> {
    Ok(Duration::ZERO)
}

#[cfg(target_os = "linux")]
fn machine_cpu() -> Option<(Duration, Duration)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    // SAFETY: `sysconf` has no preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    super::parse_proc_stat(&text, u64::try_from(ticks).ok()?)
}

#[cfg(not(target_os = "linux"))]
fn machine_cpu() -> Option<(Duration, Duration)> {
    None
}

#[cfg(target_os = "linux")]
fn disk_counters(data_dir: &str) -> Option<DiskCounters> {
    use std::os::unix::fs::MetadataExt as _;
    let dev = std::fs::metadata(data_dir).ok()?.dev();
    // glibc's gnu_dev_major / gnu_dev_minor encoding.
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    let text = std::fs::read_to_string("/proc/diskstats").ok()?;
    super::parse_diskstats(&text, major, minor)
}

#[cfg(not(target_os = "linux"))]
fn disk_counters(_data_dir: &str) -> Option<DiskCounters> {
    None
}

#[cfg(target_has_atomic = "64")]
fn run_loop_busy() -> Option<Duration> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let metrics = handle.metrics();
    (0..metrics.num_workers())
        .map(|worker| metrics.worker_total_busy_duration(worker))
        .max()
}

#[cfg(not(target_has_atomic = "64"))]
fn run_loop_busy() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_reads_this_process() {
        let provider = TokioSystemProvider::new();
        let first = provider.sample(".").expect("sample");
        let mut spin = 0_u64;
        for i in 0..2_000_000_u64 {
            spin = spin.wrapping_add(i * i);
        }
        assert_ne!(spin, 1);
        let second = provider.sample(".").expect("sample");
        assert!(second.at >= first.at);
        assert!(second.process_cpu >= first.process_cpu);
        assert!(second.machine_total >= first.machine_total);
        assert!(second.cores >= 1);
        // Outside a Tokio runtime there is no run loop to read.
        assert_eq!(second.run_loop_busy, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sample_reads_the_runtime_inside_tokio() {
        let provider = TokioSystemProvider::new();
        let sample = provider.sample(".").expect("sample");
        assert!(sample.run_loop_busy.is_some());
    }
}
