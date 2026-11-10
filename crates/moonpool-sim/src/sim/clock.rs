//! Per-process clocks: each process boot has its own `now()`.
//!
//! In production, `now()` is the time since the process started. It starts
//! near zero at each boot, and two machines cannot compare their values.
//! Without a clock model the simulation gives one clock to the whole world,
//! and that clock continues through each reboot.
//!
//! With a [`ClockModel`] ([`SimWorld::set_clock_model`]), the time provider
//! of each process reads:
//!
//! ```text
//! now = epoch + (world_now − boot_instant) × rate
//! ```
//!
//! - `epoch` is a new value at each boot, in `0..max_epoch`. A value that
//!   a process persisted can be larger than the `now()` of its next boot.
//! - `rate` is one value per machine, in `1 ± max_rate_error_ppm / 10⁶`.
//!   It applies to `now()`, to `timer()` and to each sleep and timeout: a
//!   sleep of `d` on a fast machine ends earlier in world time.
//!
//! Workloads have no process clock: they read world time.
//!
//! The epochs and rates come from a hash of the model's `salt`, the address
//! and the boot number. They make no draw from the simulation stream, so the
//! only draws are the ones that sample the model.

use std::{collections::BTreeMap, net::IpAddr, time::Duration};

use super::{SimWorld, WeakSimWorld, rng::mix64, sim_random, sim_random_bool};

/// One million: the denominator of a rate error in parts per million.
const PPM: u128 = 1_000_000;

/// The per-process clock model (see the module docs of
/// `moonpool_sim::sim::clock`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockModel {
    /// The upper bound (exclusive) of the random epoch of a boot. Zero
    /// starts each boot at zero, as `Instant::elapsed` does.
    pub max_epoch: Duration,
    /// The largest rate error of a machine clock, in parts per million.
    /// Zero gives each machine the world's rate.
    pub max_rate_error_ppm: u32,
    /// The value that the epochs and rates are hashed from.
    pub salt: u64,
}

impl ClockModel {
    /// The epoch bound of [`random_for_seed`](Self::random_for_seed).
    pub const MAX_EPOCH: Duration = Duration::from_secs(1);

    /// The rate-error bound of [`random_for_seed`](Self::random_for_seed):
    /// 5 %, much more than a real quartz clock, so that leases and failure
    /// detectors meet a clock that is clearly fast or slow.
    pub const MAX_RATE_ERROR_PPM: u32 = 50_000;

    /// A model with the given bounds and salt.
    #[must_use]
    pub const fn new(max_epoch: Duration, max_rate_error_ppm: u32, salt: u64) -> Self {
        Self {
            max_epoch,
            max_rate_error_ppm,
            salt,
        }
    }

    /// The per-seed model: one draw from the simulation stream, the salt,
    /// with the default bounds.
    #[must_use]
    pub fn random_for_seed() -> Self {
        Self::new(Self::MAX_EPOCH, Self::MAX_RATE_ERROR_PPM, sim_random())
    }

    /// The per-seed swarm draw: the model is on for half of the seeds (one
    /// draw), with [`random_for_seed`](Self::random_for_seed)'s salt (one
    /// more draw, only when on). Off is `None`: nothing changes.
    #[must_use]
    pub fn swarm_for_seed() -> Option<Self> {
        sim_random_bool(0.5).then(Self::random_for_seed)
    }

    fn hash(&self, ip: IpAddr, extra: u64) -> u64 {
        let address = match ip {
            IpAddr::V4(v4) => u64::from(v4.to_bits()),
            IpAddr::V6(v6) => {
                let bits = v6.to_bits();
                let low = u64::try_from(bits & u128::from(u64::MAX)).unwrap_or(0);
                let high = u64::try_from(bits >> 64).unwrap_or(0);
                mix64(high) ^ low
            }
        };
        mix64(mix64(self.salt ^ address) ^ extra)
    }

    /// The rate error of the machine at `ip`, in parts per million.
    fn rate_error_ppm(&self, ip: IpAddr) -> i64 {
        if self.max_rate_error_ppm == 0 {
            return 0;
        }
        let span = u64::from(self.max_rate_error_ppm) * 2 + 1;
        let offset = self.hash(ip, u64::MAX) % span;
        i64::try_from(offset).unwrap_or(0) - i64::from(self.max_rate_error_ppm)
    }

    /// The epoch of boot number `boot` of the process at `ip`.
    fn epoch(&self, ip: IpAddr, boot: u64) -> Duration {
        let bound = u64::try_from(self.max_epoch.as_nanos()).unwrap_or(u64::MAX);
        if bound == 0 {
            return Duration::ZERO;
        }
        Duration::from_nanos(self.hash(ip, boot) % bound)
    }
}

/// The clock of one process boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessClock {
    /// The world time of the boot.
    pub boot_at: Duration,
    /// The process time at the boot.
    pub epoch: Duration,
    /// The rate error of the machine, in parts per million.
    pub rate_error_ppm: i64,
}

impl ProcessClock {
    fn rate_numerator(&self) -> u128 {
        let ppm = i128::try_from(PPM).unwrap_or(i128::MAX);
        u128::try_from(ppm + i128::from(self.rate_error_ppm)).unwrap_or(1)
    }

    /// The process time at world time `world`.
    #[must_use]
    pub fn local(&self, world: Duration) -> Duration {
        let elapsed = world.saturating_sub(self.boot_at).as_nanos();
        let scaled = elapsed.saturating_mul(self.rate_numerator()) / PPM;
        self.epoch.saturating_add(Duration::from_nanos(
            u64::try_from(scaled).unwrap_or(u64::MAX),
        ))
    }

    /// The world duration of `local` process time: a sleep of `local` ends
    /// after this much world time.
    #[must_use]
    pub fn world_duration(&self, local: Duration) -> Duration {
        let numerator = self.rate_numerator().max(1);
        let world = local.as_nanos().saturating_mul(PPM).div_ceil(numerator);
        Duration::from_nanos(u64::try_from(world).unwrap_or(u64::MAX))
    }
}

/// The clock model of a world and the clock of each booted process.
#[derive(Debug, Default)]
pub(crate) struct ClockState {
    model: Option<ClockModel>,
    boots: BTreeMap<IpAddr, u64>,
    clocks: BTreeMap<IpAddr, ProcessClock>,
}

impl SimWorld {
    /// Set the per-process clock model (see the module docs of
    /// `moonpool_sim::sim::clock`). Off, which is the default, each process
    /// reads world time. The runner sets it per seed from
    /// [`Chaos::Clocks`](crate::Chaos::Clocks); a raw `SimWorld` user sets
    /// it before the first boot.
    pub fn set_clock_model(&self, model: Option<ClockModel>) {
        let mut inner = self.inner.write();
        inner.clocks.model = model;
        if model.is_none() {
            inner.clocks.clocks.clear();
        }
    }

    /// Start a new clock for the process at `ip`: its boot instant is now
    /// and its epoch is the next one of the address. It has no effect while
    /// the model is off.
    pub fn boot_process_clock(&self, ip: IpAddr) {
        let mut inner = self.inner.write();
        let Some(model) = inner.clocks.model else {
            return;
        };
        let boot_at = inner.now();
        let boot = inner.clocks.boots.entry(ip).or_insert(0);
        let number = *boot;
        *boot += 1;
        let clock = ProcessClock {
            boot_at,
            epoch: model.epoch(ip, number),
            rate_error_ppm: model.rate_error_ppm(ip),
        };
        inner.clocks.clocks.insert(ip, clock);
    }

    /// The clock of the process at `ip`, or `None` when it reads world
    /// time (the model is off, or no process booted at `ip`).
    #[must_use]
    pub fn process_clock(&self, ip: IpAddr) -> Option<ProcessClock> {
        self.inner.read().clocks.clocks.get(&ip).copied()
    }
}

impl WeakSimWorld {
    /// The clock of the process at `ip` (see [`SimWorld::process_clock`]).
    pub(crate) fn process_clock(&self, ip: IpAddr) -> Option<ProcessClock> {
        self.upgrade().ok()?.process_clock(ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(rate_error_ppm: i64) -> ProcessClock {
        ProcessClock {
            boot_at: Duration::from_secs(100),
            epoch: Duration::from_secs(3),
            rate_error_ppm,
        }
    }

    #[test]
    fn a_process_clock_starts_at_its_epoch_and_runs_at_its_rate() {
        let fast = clock(50_000);
        assert_eq!(fast.local(Duration::from_secs(100)), Duration::from_secs(3));
        assert_eq!(
            fast.local(Duration::from_secs(110)),
            Duration::from_millis(13_500)
        );
        let slow = clock(-50_000);
        assert_eq!(
            slow.local(Duration::from_secs(110)),
            Duration::from_millis(12_500)
        );
    }

    #[test]
    fn a_sleep_on_a_fast_clock_takes_less_world_time() {
        let fast = clock(50_000);
        let world = fast.world_duration(Duration::from_millis(1_050));
        assert_eq!(world, Duration::from_secs(1));
        // The sleep never ends before its local duration passed.
        assert!(fast.local(fast.boot_at + world) >= fast.epoch + Duration::from_millis(1_050));
        let slow = clock(-50_000);
        assert!(slow.world_duration(Duration::from_secs(1)) > Duration::from_secs(1));
    }

    #[test]
    fn epochs_and_rates_are_in_their_bounds() {
        let model = ClockModel::new(Duration::from_secs(10), 50_000, 7);
        for host in 1..=50u8 {
            let ip = IpAddr::from([10, 0, 1, host]);
            assert!(model.rate_error_ppm(ip).abs() <= 50_000);
            for boot in 0..5 {
                assert!(model.epoch(ip, boot) < Duration::from_secs(10));
            }
        }
        let flat = ClockModel::new(Duration::ZERO, 0, 7);
        let ip = IpAddr::from([10, 0, 1, 1]);
        assert_eq!(flat.epoch(ip, 3), Duration::ZERO);
        assert_eq!(flat.rate_error_ppm(ip), 0);
    }
}
