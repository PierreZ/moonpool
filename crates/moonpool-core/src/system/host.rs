//! The host readers behind every production [`SystemProvider`](super::SystemProvider).
//!
//! [`sample`] reads every [`SystemSample`] field from the kernel except
//! `run_loop_busy`, the one that depends on the runtime: a backend calls it,
//! then fills that field its own way (Tokio's worker metrics, compio's
//! [`rusage_cpu`] of the runtime thread).
//!
//! These are host-side helpers shared by the production system backends,
//! not part of the provider API: that is why they sit at the crate root
//! behind the `host` feature, apart from the system traits and types.

use std::{io, time::Duration};

use super::{DiskCounters, DiskSpace, SystemSample};

/// Read the host's counters, with `at` as the sample's
/// [`SystemSample::at`] and `data_dir` naming the disk to report.
///
/// - `process_cpu`: `getrusage(RUSAGE_SELF)`, user plus system (Unix).
/// - `machine_busy` / `machine_total`: the `cpu` line of `/proc/stat`
///   (Linux); elsewhere zero busy time over `at`.
/// - `disk`: the `/proc/diskstats` line of the device that holds `data_dir`,
///   found by the major and minor numbers of its `stat()` (Linux), the
///   method FDB uses. `None` when no line matches, which happens on
///   btrfs, overlayfs and some container setups.
/// - `space`: `statvfs()` of `data_dir` (Unix): `f_blocks` and `f_bavail`
///   times `f_frsize`.
/// - `cores`: [`std::thread::available_parallelism`], which honours the
///   cgroup CPU quota and the affinity mask.
/// - `run_loop_busy`: always `None`; the backend fills it.
///
/// # Errors
///
/// The OS error when `getrusage` fails. Every other reader degrades to a
/// fallback or `None` instead.
#[tracing::instrument(level = "trace")]
pub fn sample(at: Duration, data_dir: &str) -> io::Result<SystemSample> {
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
        run_loop_busy: None,
        disk: disk_counters(data_dir),
        space: disk_space(data_dir),
    })
}

/// Whose CPU time [`rusage_cpu`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RusageWho {
    /// The whole process (`RUSAGE_SELF`).
    Process,
    /// The calling thread (`RUSAGE_THREAD`, Linux only).
    #[cfg(target_os = "linux")]
    Thread,
}

/// User plus system CPU time of `who`, from `getrusage`. Zero off unix.
///
/// # Errors
///
/// The OS error when `getrusage` fails.
#[cfg(unix)]
pub fn rusage_cpu(who: RusageWho) -> io::Result<Duration> {
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

/// User plus system CPU time of `who`, from `getrusage`. Zero off unix.
///
/// # Errors
///
/// Never fails off unix.
#[cfg(not(unix))]
pub fn rusage_cpu(_who: RusageWho) -> io::Result<Duration> {
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
