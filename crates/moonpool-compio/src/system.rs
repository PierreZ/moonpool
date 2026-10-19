//! CPU and disk counters on a compio deployment: [`CompioSystemProvider`].
//!
//! The machine and disk readers are copies of `moonpool-core`'s Tokio
//! backend (`crates/moonpool-core/src/system/tokio_impl.rs`), which keeps
//! them private; keep the two in step.

use std::io;
use std::time::Duration;

use moonpool_core::{DiskCounters, DiskSpace, SystemProvider, SystemSample};

/// System provider for thread-per-core deployments on compio.
///
/// Every field is read the way `TokioSystemProvider` reads it, except the
/// run loop:
///
/// - `run_loop_busy`: the CPU time (user plus system,
///   `getrusage(RUSAGE_THREAD)`) of the calling thread, when it runs a
///   compio runtime. With one runtime per core, that thread *is* the run
///   loop, and its CPU time is the honest measure of how busy it is (time
///   parked in the kernel waiting for completions is not CPU time). Call
///   [`sample`](SystemProvider::sample) from a task on the runtime you want
///   measured. `None` outside a compio runtime and off Linux.
#[derive(Debug, Clone)]
pub struct CompioSystemProvider {
    start: std::time::Instant,
}

impl CompioSystemProvider {
    /// Create a system provider. Its [`SystemSample::at`] counts from now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for CompioSystemProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemProvider for CompioSystemProvider {
    fn sample(&self, data_dir: &str) -> io::Result<SystemSample> {
        let at = self.start.elapsed();
        let process_cpu = rusage_cpu(RusageWho::Process)?;
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
            space: disk_space(data_dir),
        })
    }
}

/// Whose CPU time `getrusage` reads.
#[derive(Debug, Clone, Copy)]
enum RusageWho {
    Process,
    #[cfg(target_os = "linux")]
    Thread,
}

#[cfg(unix)]
fn rusage_cpu(who: RusageWho) -> io::Result<Duration> {
    let who = match who {
        RusageWho::Process => libc::RUSAGE_SELF,
        #[cfg(target_os = "linux")]
        RusageWho::Thread => libc::RUSAGE_THREAD,
    };
    // SAFETY: `getrusage` only writes into the zeroed struct we own.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer to a properly sized `rusage`.
    let rc = unsafe { libc::getrusage(who, &raw mut usage) };
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
fn rusage_cpu(_who: RusageWho) -> io::Result<Duration> {
    Ok(Duration::ZERO)
}

/// The calling thread's CPU time, when that thread runs a compio runtime.
#[cfg(target_os = "linux")]
fn run_loop_busy() -> Option<Duration> {
    compio::runtime::Runtime::try_with_current(|_| ()).ok()?;
    rusage_cpu(RusageWho::Thread).ok()
}

#[cfg(not(target_os = "linux"))]
fn run_loop_busy() -> Option<Duration> {
    None
}

#[cfg(target_os = "linux")]
fn machine_cpu() -> Option<(Duration, Duration)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    // SAFETY: `sysconf` has no preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    moonpool_core::parse_proc_stat(&text, u64::try_from(ticks).ok()?)
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
    moonpool_core::parse_diskstats(&text, major, minor)
}

#[cfg(not(target_os = "linux"))]
fn disk_counters(_data_dir: &str) -> Option<DiskCounters> {
    None
}

#[cfg(unix)]
fn disk_space(data_dir: &str) -> Option<DiskSpace> {
    let path = std::ffi::CString::new(data_dir).ok()?;
    // SAFETY: `statvfs` only writes into the zeroed struct we own.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and a properly sized `statvfs`.
    let rc = unsafe { libc::statvfs(path.as_ptr(), &raw mut stat) };
    if rc != 0 {
        return None;
    }
    let block = widen(stat.f_frsize);
    let (blocks, available) = (widen(stat.f_blocks), widen(stat.f_bavail));
    Some(DiskSpace {
        total_bytes: blocks.saturating_mul(block),
        free_bytes: available.saturating_mul(block),
    })
}

/// The `statvfs` field widths differ per target: widen each one to `u64`.
#[cfg(unix)]
fn widen(value: impl Into<u64>) -> u64 {
    value.into()
}

#[cfg(not(unix))]
fn disk_space(_data_dir: &str) -> Option<DiskSpace> {
    None
}

#[cfg(test)]
mod tests {
    use super::{CompioSystemProvider, SystemProvider};

    #[test]
    fn outside_a_runtime_there_is_no_run_loop() {
        let sample = CompioSystemProvider::new().sample(".").expect("sample");
        assert!(sample.cores >= 1);
        assert_eq!(sample.run_loop_busy, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inside_a_runtime_the_run_loop_is_this_thread() {
        let runtime = compio::runtime::Runtime::new().expect("runtime");
        let busy = runtime.block_on(async {
            let provider = CompioSystemProvider::new();
            let first = provider.sample(".").expect("sample").run_loop_busy;
            let mut spin = 0_u64;
            for i in 0..2_000_000_u64 {
                spin = spin.wrapping_add(i * i);
            }
            assert_ne!(spin, 1);
            let second = provider.sample(".").expect("sample").run_loop_busy;
            (first, second)
        });
        let (Some(first), Some(second)) = busy else {
            panic!("a compio runtime thread reports its CPU time: {busy:?}");
        };
        assert!(second >= first);
    }
}
