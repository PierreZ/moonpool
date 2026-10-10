//! Simulation system provider: deterministic disk counters per process.

use std::{io, net::IpAddr};

use moonpool_core::{SystemProvider, SystemSample};

use crate::sim::WeakSimWorld;

/// Simulation system provider, scoped to one process IP.
///
/// The disk counters are the simulated disk's own (see
/// [`SimWorld::disk_counters`](crate::SimWorld::disk_counters)): the time
/// with at least one operation in flight, the completed reads and writes,
/// their bytes and their times. They are exact and deterministic, and a disk
/// stall or throttle episode makes them rise.
///
/// A task poll takes no simulated time today, so the CPU counters are zero:
/// `process_cpu`, `machine_busy` and `run_loop_busy` stay at zero and
/// `machine_total` follows the clock of one core.
///
/// The world remembers the last samples it handed out per process
/// ([`SimWorld::system_samples`](crate::SimWorld::system_samples)), so a
/// harness can check a window a process reports against the samples it was
/// really given.
#[derive(Debug, Clone)]
pub struct SimSystemProvider {
    sim: WeakSimWorld,
    ip: IpAddr,
}

impl SimSystemProvider {
    /// Create a system provider for the process at `ip`.
    #[must_use]
    pub fn new(sim: WeakSimWorld, ip: IpAddr) -> Self {
        Self { sim, ip }
    }
}

impl SystemProvider for SimSystemProvider {
    fn sample(&self, _data_dir: &str) -> io::Result<SystemSample> {
        let sim = self
            .sim
            .upgrade()
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(sim.system_sample(self.ip))
    }
}
