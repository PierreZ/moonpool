//! Production [`SystemProvider`]: `getrusage`, `/proc/stat`, `/proc/diskstats`
//! and Tokio's runtime metrics.

use std::{io, time::Duration};

use super::{SystemProvider, SystemSample};

/// Production system provider.
///
/// - `process_cpu`: `getrusage(RUSAGE_SELF)`, user plus system (Unix).
/// - `machine_busy` / `machine_total`: the `cpu` line of `/proc/stat` (Linux).
/// - `disk`: the `/proc/diskstats` line of the device that holds `data_dir`,
///   found by the major and minor numbers of its `stat()` (Linux), the
///   method FDB uses. `None` when no line matches, which happens on
///   btrfs, overlayfs and some container setups.
/// - `space`: `statvfs()` of `data_dir` (Unix): `f_blocks` and `f_bavail`
///   times `f_frsize`.
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
        let mut sample = super::host::sample(self.start.elapsed(), data_dir)?;
        sample.run_loop_busy = run_loop_busy();
        Ok(sample)
    }
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
        let space = second.space.expect("statvfs reads the current directory");
        assert!(space.free_bytes <= space.total_bytes);
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
