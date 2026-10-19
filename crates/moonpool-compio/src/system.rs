//! CPU and disk counters on a compio deployment: [`CompioSystemProvider`].
//!
//! The process, machine and disk readers are `moonpool-core`'s shared
//! [`moonpool_core::host`] ones; only the run loop is read here.

use std::io;
use std::time::Duration;

#[cfg(target_os = "linux")]
use moonpool_core::host::{RusageWho, rusage_cpu};
use moonpool_core::{SystemProvider, SystemSample, host};

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
        let mut sample = host::sample(self.start.elapsed(), data_dir)?;
        sample.run_loop_busy = run_loop_busy();
        Ok(sample)
    }
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
