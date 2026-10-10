//! System counters provider: CPU and disk activity of the running process.
//!
//! A [`SystemProvider`] reads **cumulative** counters, the way the kernel
//! exposes them (`getrusage`, `/proc/stat`, `/proc/diskstats`). It does no
//! arithmetic: a caller takes two [`SystemSample`]s and divides the
//! differences by the elapsed time to get busyness ratios, as FDB's
//! `getSystemStatistics` does (`flow/Platform.cpp`).
//!
//! In simulation the counters are deterministic: the simulated disk counts
//! the time it has at least one operation in flight, per process, so a disk
//! stall or throttle episode makes it rise. FDB turns its system
//! monitor off in simulation; moonpool does not, so the code that reads the
//! counters runs under simulation too.

use std::{io, time::Duration};

#[cfg(feature = "tokio-system")]
mod tokio_impl;

#[cfg(feature = "tokio-system")]
pub use tokio_impl::TokioSystemProvider;

/// Provider of cumulative CPU and disk counters.
///
/// Every counter in a [`SystemSample`] only grows while the process lives.
/// A counter that goes back (a process reboot, a wrapped kernel counter)
/// tells the caller that the window between two samples is not valid.
pub trait SystemProvider: Clone + Send + Sync + 'static {
    /// Read the counters now.
    ///
    /// `data_dir` selects the disk: the counters are those of the block
    /// device that holds this directory. The simulation has one disk per
    /// process and ignores the path.
    ///
    /// # Errors
    ///
    /// Returns an error when a process counter cannot be read. A disk that
    /// cannot be resolved is not an error: [`SystemSample::disk`] is `None`.
    fn sample(&self, data_dir: &str) -> io::Result<SystemSample>;
}

/// One reading of the cumulative counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSample {
    /// Monotonic instant of the reading. Only differences have a meaning.
    pub at: Duration,
    /// CPU time of this process, user plus system.
    pub process_cpu: Duration,
    /// Logical cores this process may use (cgroup quota and affinity).
    pub cores: u32,
    /// Busy CPU time of the whole machine, summed over its cores
    /// (`/proc/stat`: total minus idle minus iowait).
    pub machine_busy: Duration,
    /// Total CPU time of the whole machine, summed over its cores.
    pub machine_total: Duration,
    /// Busy time of the run loop: the busiest worker thread under Tokio, the
    /// process's CPU under simulation. `None` when the runtime does not
    /// expose it.
    pub run_loop_busy: Option<Duration>,
    /// Counters of the disk that holds the data directory. `None` when the
    /// device is not known (an overlay or btrfs mount, another OS).
    pub disk: Option<DiskCounters>,
}

/// Cumulative counters of one disk, as in `/proc/diskstats`.
///
/// `io_ticks` is the time with at least one I/O in flight, which is iostat's
/// `%util` once divided by the elapsed time. On flash and RAID devices it
/// reaches 100% long before the device reaches its throughput limit, and
/// since Linux 5.0 the kernel only approximates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCounters {
    /// Name of the resolved device, for example `nvme0n1p1`.
    pub device: String,
    /// Time with at least one I/O in flight.
    pub io_ticks: Duration,
    /// I/Os in flight now.
    pub in_flight: u64,
    /// Completed reads.
    pub reads: u64,
    /// Completed writes.
    pub writes: u64,
    /// Bytes read by completed reads.
    pub read_bytes: u64,
    /// Bytes written by completed writes.
    pub write_bytes: u64,
    /// Sum of the durations of the completed reads.
    pub read_time: Duration,
    /// Sum of the durations of the completed writes.
    pub write_time: Duration,
}

/// Bytes in one `/proc/diskstats` sector (always 512, whatever the device).
const DISKSTATS_SECTOR: u64 = 512;

/// Parse the aggregate `cpu` line of `/proc/stat` into `(busy, total)`.
///
/// `ticks_per_second` is `sysconf(_SC_CLK_TCK)`. Busy is total minus idle
/// minus iowait, as FDB computes it. The guest fields are already
/// counted in user and nice, so they are left out of the total.
#[must_use]
pub fn parse_proc_stat(text: &str, ticks_per_second: u64) -> Option<(Duration, Duration)> {
    if ticks_per_second == 0 {
        return None;
    }
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let fields = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    // user nice system idle iowait irq softirq steal: kernels since 2.6.11.
    if fields.len() < 5 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3].saturating_add(fields[4]);
    let busy = total.saturating_sub(idle);
    let to_duration = |ticks: u64| {
        Duration::from_secs(ticks / ticks_per_second)
            + Duration::from_nanos((ticks % ticks_per_second) * 1_000_000_000 / ticks_per_second)
    };
    Some((to_duration(busy), to_duration(total)))
}

/// Parse the `/proc/diskstats` line of the device `major:minor`.
#[must_use]
pub fn parse_diskstats(text: &str, major: u64, minor: u64) -> Option<DiskCounters> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let line_major = fields.next()?.parse::<u64>().ok()?;
        let line_minor = fields.next()?.parse::<u64>().ok()?;
        if (line_major, line_minor) != (major, minor) {
            return None;
        }
        let device = fields.next()?.to_string();
        let values = fields
            .take(10)
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        if values.len() < 10 {
            return None;
        }
        // Fields 1..=10 after the name: reads, reads merged, sectors read,
        // ms reading, writes, writes merged, sectors written, ms writing,
        // I/Os in flight, ms with I/O in flight (io_ticks).
        Some(DiskCounters {
            device,
            reads: values[0],
            read_bytes: values[2].saturating_mul(DISKSTATS_SECTOR),
            read_time: Duration::from_millis(values[3]),
            writes: values[4],
            write_bytes: values[6].saturating_mul(DISKSTATS_SECTOR),
            write_time: Duration::from_millis(values[7]),
            in_flight: values[8],
            io_ticks: Duration::from_millis(values[9]),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_busy_excludes_idle_and_iowait() {
        let text = "cpu  100 20 30 400 50 6 4 0 7 0\ncpu0 1 2 3 4 5 6 7 0 0 0\n";
        let (busy, total) = parse_proc_stat(text, 100).expect("parsed");
        assert_eq!(total, Duration::from_millis(6100));
        assert_eq!(busy, Duration::from_millis(1600));
    }

    #[test]
    fn proc_stat_refuses_garbage() {
        assert_eq!(parse_proc_stat("cpu  a b c", 100), None);
        assert_eq!(parse_proc_stat("intr 1 2 3", 100), None);
        assert_eq!(parse_proc_stat("cpu  1 2 3 4 5", 0), None);
    }

    #[test]
    fn diskstats_selects_the_device() {
        let text = "   8       0 sda 10 0 80 5 20 0 160 9 1 30 40 0 0 0 0\n \
                    259       1 nvme0n1p1 1200 3 9600 400 3400 7 104000 900 14 4800 5300 0 0 0 0\n";
        let disk = parse_diskstats(text, 259, 1).expect("found");
        assert_eq!(disk.device, "nvme0n1p1");
        assert_eq!(disk.reads, 1200);
        assert_eq!(disk.read_bytes, 9600 * 512);
        assert_eq!(disk.read_time, Duration::from_millis(400));
        assert_eq!(disk.writes, 3400);
        assert_eq!(disk.write_bytes, 104_000 * 512);
        assert_eq!(disk.write_time, Duration::from_millis(900));
        assert_eq!(disk.in_flight, 14);
        assert_eq!(disk.io_ticks, Duration::from_millis(4800));
        assert_eq!(parse_diskstats(text, 8, 1), None);
    }
}
