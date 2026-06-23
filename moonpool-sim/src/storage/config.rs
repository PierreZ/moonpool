//! # Storage Simulation Configuration
//!
//! This module provides configuration for storage simulation, following
//! FoundationDB's battle-tested simulation approach and TigerBeetle's deterministic
//! testing patterns.
//!
//! ## Performance Parameters
//!
//! | Parameter | Config Field | Default | Description |
//! |-----------|--------------|---------|-------------|
//! | IOPS | `iops` | 25,000 | I/O operations per second limit |
//! | Bandwidth | `bandwidth` | 150 MB/s | Maximum throughput in bytes/sec |
//! | Read latency | `read_latency` | 50-200µs | Time for read operations |
//! | Write latency | `write_latency` | 100-500µs | Time for write operations |
//! | Sync latency | `sync_latency` | 1-5ms | Time for sync operations |
//!
//! ## Fault Injection
//!
//! | Fault | Config Field | Default | Real-World Scenario |
//! |-------|--------------|---------|---------------------|
//! | Read fault | `read_fault_probability` | 0% | Disk read errors, ECC failures |
//! | Write fault | `write_fault_probability` | 0% | Write failures, disk full |
//! | Crash fault | `crash_fault_probability` | 0% | Sudden power loss simulation |
//! | Misdirected write | `misdirect_write_probability` | 0% | Write lands at wrong location |
//! | Misdirected read | `misdirect_read_probability` | 0% | Read returns wrong data |
//! | Phantom write | `phantom_write_probability` | 0% | Write appears to succeed but doesn't persist |
//! | Sync failure | `sync_failure_probability` | 0% | fsync fails |
//!
//! ## Dynamic Disk Degradation Episodes
//!
//! Real disks degrade *episodically* rather than at a fixed steady-state rate.
//! These knobs are off by default (probabilities 0, no-op multipliers); when
//! enabled they make a file enter a stall or throttle episode for a duration,
//! surfacing timeout cascades and backpressure collapse. FDB ref:
//! `DiskFailureInjector` / `getDiskDelay()`.
//!
//! | Episode | Config Field | Default | Effect while active |
//! |---------|--------------|---------|---------------------|
//! | Stall | `disk_stall_probability` / `disk_stall_duration` | 0% | Disk frozen until expiry; I/O waits out the window |
//! | Throttle | `disk_throttle_probability` / `disk_throttle_duration` | 0% | Effective IOPS/bandwidth divided by the multipliers |
//! | Throttle factor | `disk_throttle_iops_multiplier` / `disk_throttle_bandwidth_multiplier` | 1.0 | Divisor applied to IOPS / bandwidth |
//!
//! ## Configuration Examples
//!
//! ### Fast Local Testing (No Chaos)
//! ```rust
//! use moonpool_sim::storage::StorageConfiguration;
//!
//! let config = StorageConfiguration::fast_local();
//! // All faults disabled, minimal latencies
//! ```
//!
//! ### Full Chaos Testing
//! ```rust
//! use moonpool_sim::storage::StorageConfiguration;
//!
//! let config = StorageConfiguration::random_for_seed();
//! // Randomized fault parameters for comprehensive testing
//! ```
//!
//! ## FDB/TigerBeetle References
//!
//! - Simulated file operations: FDB sim2.actor.cpp
//! - Storage faults: TigerBeetle storage simulation
//! - Crash consistency: FDB AsyncFileKAIO, TigerBeetle deterministic testing

use crate::network::config::{LatencyDistribution, random_latency_for_seed};
use crate::sim::rng::{config_random_bool, sim_random_range};
use std::time::Duration;

/// Configuration for storage simulation parameters.
///
/// This struct contains all settings related to storage simulation including
/// performance characteristics and fault injection probabilities.
#[derive(Debug, Clone)]
pub struct StorageConfiguration {
    // =========================================================================
    // Performance Parameters
    // =========================================================================
    /// I/O operations per second limit.
    ///
    /// Typical values:
    /// - `NVMe` SSD: 100,000-500,000 IOPS
    /// - SATA SSD: 25,000-100,000 IOPS
    /// - HDD: 100-200 IOPS
    pub iops: u64,

    /// Maximum bandwidth in bytes per second.
    ///
    /// Typical values:
    /// - `NVMe` SSD: 3,000-7,000 MB/s
    /// - SATA SSD: 500-600 MB/s
    /// - HDD: 100-200 MB/s
    pub bandwidth: u64,

    /// Latency distribution for read operations.
    ///
    /// Typical values:
    /// - `NVMe` SSD: 20-100µs
    /// - SATA SSD: 50-200µs
    /// - HDD: 2-10ms
    pub read_latency: LatencyDistribution,

    /// Latency distribution for write operations.
    ///
    /// Typical values:
    /// - `NVMe` SSD: 20-100µs
    /// - SATA SSD: 100-500µs
    /// - HDD: 2-10ms
    pub write_latency: LatencyDistribution,

    /// Latency distribution for sync/flush operations.
    ///
    /// Sync operations ensure data durability and typically take
    /// longer than regular read/write operations.
    pub sync_latency: LatencyDistribution,

    // =========================================================================
    // Fault Injection Probabilities
    // =========================================================================
    /// Probability of read operation failing (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Simulates disk read errors, ECC failures, or media degradation.
    pub read_fault_probability: f64,

    /// Probability of write operation failing (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Simulates write failures due to disk full, bad sectors, or media errors.
    pub write_fault_probability: f64,

    /// Probability of crash fault during operation (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Simulates sudden power loss or system crash during I/O.
    /// Tests crash consistency and recovery logic.
    pub crash_fault_probability: f64,

    /// Probability of write landing at wrong location (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Simulates misdirected writes where data is written to a different
    /// block than intended. Tests checksum validation and corruption detection.
    /// `TigerBeetle` ref: storage fault injection
    pub misdirect_write_probability: f64,

    /// Probability of read returning data from wrong location (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Simulates misdirected reads where data is read from a different
    /// block than intended. Tests checksum validation.
    /// `TigerBeetle` ref: storage fault injection
    pub misdirect_read_probability: f64,

    /// Probability of write appearing to succeed but not persisting (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Simulates phantom writes where the write reports success but data
    /// is lost before reaching stable storage. Tests durability guarantees.
    pub phantom_write_probability: f64,

    /// Probability of sync/flush operation failing (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Simulates fsync failures which can indicate serious storage issues.
    /// Tests error handling in durability-critical code paths.
    pub sync_failure_probability: f64,

    // =========================================================================
    // Dynamic Disk Degradation Episodes
    // =========================================================================
    // Real disks degrade *episodically* rather than at a fixed steady-state rate:
    // brief full stalls (GC/thermal/firmware pauses) and longer throttle periods
    // (reduced IOPS/bandwidth). These surface timeout cascades and backpressure
    // collapse that steady-state timing never produces. Off by default (all 0).
    // FDB ref: `DiskFailureInjector` / `getDiskDelay()`.
    /// Per-operation probability of entering a *stall* episode (0.0 - 1.0).
    ///
    /// During a stall the disk is frozen until the episode expires; any I/O
    /// scheduled in that window waits out the remaining time before completing.
    pub disk_stall_probability: f64,

    /// How long a stall episode freezes the disk once entered.
    pub disk_stall_duration: Duration,

    /// Per-operation probability of entering a *throttle* episode (0.0 - 1.0).
    ///
    /// During a throttle the effective IOPS/bandwidth are divided by the
    /// configured multipliers for the duration of the episode.
    pub disk_throttle_probability: f64,

    /// How long a throttle episode reduces throughput once entered.
    pub disk_throttle_duration: Duration,

    /// Divisor applied to effective IOPS during a throttle episode (>= 1.0).
    ///
    /// E.g. `10.0` makes the disk handle one tenth its normal IOPS.
    pub disk_throttle_iops_multiplier: f64,

    /// Divisor applied to effective bandwidth during a throttle episode (>= 1.0).
    pub disk_throttle_bandwidth_multiplier: f64,
}

impl Default for StorageConfiguration {
    fn default() -> Self {
        Self {
            // Performance parameters matching a typical SATA SSD
            iops: 25_000,
            bandwidth: 150_000_000, // 150 MB/s
            read_latency: LatencyDistribution::Uniform {
                start: Duration::from_micros(50),
                end: Duration::from_micros(200),
            },
            write_latency: LatencyDistribution::Uniform {
                start: Duration::from_micros(100),
                end: Duration::from_micros(500),
            },
            sync_latency: LatencyDistribution::Uniform {
                start: Duration::from_millis(1),
                end: Duration::from_millis(5),
            },

            // Fault probabilities - disabled by default for predictable behavior
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,

            // Dynamic disk degradation - disabled by default (no-op multipliers)
            disk_stall_probability: 0.0,
            disk_stall_duration: Duration::ZERO,
            disk_throttle_probability: 0.0,
            disk_throttle_duration: Duration::ZERO,
            disk_throttle_iops_multiplier: 1.0,
            disk_throttle_bandwidth_multiplier: 1.0,
        }
    }
}

impl StorageConfiguration {
    /// Create a new storage configuration with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a randomized storage configuration for chaos testing.
    ///
    /// Uses deterministic random values based on the simulation seed
    /// to create varied configurations across test runs.
    #[must_use]
    pub fn random_for_seed() -> Self {
        Self {
            // Randomize IOPS between 10,000 and 100,000
            iops: sim_random_range(10_000..100_000),

            // Randomize bandwidth between 50 MB/s and 500 MB/s
            bandwidth: sim_random_range(50_000_000..500_000_000),

            // Randomize latencies, mixing distribution shapes per field
            read_latency: random_latency_for_seed(
                Duration::from_micros(sim_random_range(20..100))
                    ..Duration::from_micros(sim_random_range(100..500)),
            ),
            write_latency: random_latency_for_seed(
                Duration::from_micros(sim_random_range(50..200))
                    ..Duration::from_micros(sim_random_range(200..1000)),
            ),
            sync_latency: random_latency_for_seed(
                Duration::from_micros(sim_random_range(500..2000))
                    ..Duration::from_micros(sim_random_range(2000..10000)),
            ),

            // Low fault probabilities for chaos testing (0.001% to 0.1%)
            read_fault_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            write_fault_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            crash_fault_probability: f64::from(sim_random_range(0..50)) / 100_000.0,
            misdirect_write_probability: f64::from(sim_random_range(0..10)) / 100_000.0,
            misdirect_read_probability: f64::from(sim_random_range(0..10)) / 100_000.0,
            phantom_write_probability: f64::from(sim_random_range(0..20)) / 100_000.0,
            sync_failure_probability: f64::from(sim_random_range(0..50)) / 100_000.0,

            // Low-rate disk-degradation episodes (drawn after the faults so the
            // existing per-field RNG sub-sequence is unchanged).
            disk_stall_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            disk_stall_duration: Duration::from_millis(sim_random_range(50..500)),
            disk_throttle_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            disk_throttle_duration: Duration::from_millis(sim_random_range(500..5000)),
            disk_throttle_iops_multiplier: f64::from(sim_random_range(2..20)),
            disk_throttle_bandwidth_multiplier: f64::from(sim_random_range(2..20)),
        }
    }

    /// Create a swarm-testing storage configuration for seed-based testing.
    ///
    /// Starts from [`random_for_seed`](Self::random_for_seed), then disables each
    /// fault family with ~50% probability (drawn from the independent `CONFIG_RNG`
    /// stream). This implements *swarm testing* (Groce et al., ISSTA 2012): each
    /// seed exercises a random *subset* of storage fault families — including the
    /// all-off subset — instead of every family being slightly on at once (which
    /// lets families crowd each other out, the passive-suppression anti-pattern).
    #[must_use]
    pub fn swarm_for_seed() -> Self {
        let mut config = Self::random_for_seed();
        config.apply_swarm_mask();
        config
    }

    /// Disable each fault family with ~50% probability using the `CONFIG_RNG`
    /// stream (see [`swarm_for_seed`](Self::swarm_for_seed)).
    ///
    /// Draws exactly nine `config_random_bool` values (one per family: the seven
    /// per-op faults plus the stall and throttle episode families) so the
    /// `CONFIG_RNG` call sequence is fixed and reproducible per seed. Performance
    /// parameters (IOPS, bandwidth, latencies) stay as sampled — only the fault
    /// families are masked.
    fn apply_swarm_mask(&mut self) {
        if !config_random_bool(0.5) {
            self.read_fault_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.write_fault_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.crash_fault_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.misdirect_read_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.misdirect_write_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.phantom_write_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.sync_failure_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.disk_stall_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.disk_throttle_probability = 0.0;
        }
    }

    /// Spike selected disk knob *magnitudes* under buggify (FDB's
    /// `if (randomize && BUGGIFY) KNOB = random(lo, hi)`).
    ///
    /// Composes on top of [`random_for_seed`](Self::random_for_seed) /
    /// [`swarm_for_seed`](Self::swarm_for_seed): each knob keeps its sampled value
    /// unless its own [`buggify_knob!`](crate::buggify_knob) call site fires for the
    /// seed. Demonstrates both throttling *down* (IOPS/bandwidth → extreme-slow
    /// disk) and spiking a fault rate *up*. A representative subset — extend by
    /// adding more `buggify_knob!` lines.
    pub fn apply_buggify_knobs(&mut self) {
        self.iops = crate::buggify_knob!(self.iops, 100..5_000);
        self.bandwidth = crate::buggify_knob!(self.bandwidth, 1_000_000..20_000_000);
        self.sync_failure_probability =
            crate::buggify_knob!(self.sync_failure_probability, 0.05..0.2);
        self.disk_stall_probability = crate::buggify_knob!(self.disk_stall_probability, 0.1..0.5);
        self.disk_stall_duration = crate::buggify_knob!(
            self.disk_stall_duration,
            Duration::from_millis(100)..Duration::from_millis(500)
        );
        self.disk_throttle_probability =
            crate::buggify_knob!(self.disk_throttle_probability, 0.1..0.5);
        self.disk_throttle_duration = crate::buggify_knob!(
            self.disk_throttle_duration,
            Duration::from_secs(1)..Duration::from_secs(5)
        );
    }

    /// Create a configuration optimized for fast local testing.
    ///
    /// Minimal latencies and no fault injection for predictable,
    /// fast test execution.
    #[must_use]
    pub fn fast_local() -> Self {
        let one_us = Duration::from_micros(1);
        let uniform = LatencyDistribution::Uniform {
            start: one_us,
            end: one_us,
        };
        Self {
            iops: 1_000_000,          // Very high IOPS
            bandwidth: 1_000_000_000, // 1 GB/s
            read_latency: uniform.clone(),
            write_latency: uniform.clone(),
            sync_latency: uniform,

            // All faults disabled
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,

            // Disk degradation disabled (no-op multipliers)
            disk_stall_probability: 0.0,
            disk_stall_duration: Duration::ZERO,
            disk_throttle_probability: 0.0,
            disk_throttle_duration: Duration::ZERO,
            disk_throttle_iops_multiplier: 1.0,
            disk_throttle_bandwidth_multiplier: 1.0,
        }
    }
}

#[cfg(test)]
mod swarm_tests {
    use super::StorageConfiguration;
    use crate::sim::rng::{reset_sim_rng, set_config_seed, set_sim_seed};

    /// The on/off state of each swarmed fault family, in mask order.
    fn enabled_families(config: &StorageConfiguration) -> [bool; 9] {
        [
            config.read_fault_probability > 0.0,
            config.write_fault_probability > 0.0,
            config.crash_fault_probability > 0.0,
            config.misdirect_read_probability > 0.0,
            config.misdirect_write_probability > 0.0,
            config.phantom_write_probability > 0.0,
            config.sync_failure_probability > 0.0,
            config.disk_stall_probability > 0.0,
            config.disk_throttle_probability > 0.0,
        ]
    }

    /// Build a swarm config the way the runner does: both streams seeded per iteration.
    fn swarm_for(seed: u64) -> StorageConfiguration {
        reset_sim_rng();
        set_sim_seed(seed);
        set_config_seed(seed);
        StorageConfiguration::swarm_for_seed()
    }

    #[test]
    fn swarm_subset_is_deterministic_per_seed() {
        for seed in [0_u64, 1, 42, 12_345] {
            let first = enabled_families(&swarm_for(seed));
            let second = enabled_families(&swarm_for(seed));
            assert_eq!(
                first, second,
                "swarm subset must be reproducible for seed {seed}"
            );
        }
    }

    #[test]
    fn swarm_reaches_all_off_and_mixed_subsets() {
        let mut saw_all_off = false;
        let mut saw_mixed = false;

        for seed in 0..1000_u64 {
            let families = enabled_families(&swarm_for(seed));
            let on = families.iter().filter(|&&e| e).count();
            if on == 0 {
                saw_all_off = true;
            }
            if on > 0 && on < families.len() {
                saw_mixed = true;
            }
            if saw_all_off && saw_mixed {
                break;
            }
        }

        assert!(
            saw_all_off,
            "no seed in 0..1000 produced the all-off subset"
        );
        assert!(saw_mixed, "no seed in 0..1000 produced a mixed subset");
    }

    #[test]
    fn swarm_all_off_seed_has_zero_fault_probabilities() {
        // Find a seed whose subset is entirely off, then assert every family is inert.
        let seed = (0..1000_u64)
            .find(|&s| enabled_families(&swarm_for(s)).iter().all(|&e| !e))
            .expect("expected an all-off seed within 0..1000");

        let config = swarm_for(seed);
        assert_zero(config.read_fault_probability);
        assert_zero(config.write_fault_probability);
        assert_zero(config.crash_fault_probability);
        assert_zero(config.misdirect_read_probability);
        assert_zero(config.misdirect_write_probability);
        assert_zero(config.phantom_write_probability);
        assert_zero(config.sync_failure_probability);
        assert_zero(config.disk_stall_probability);
        assert_zero(config.disk_throttle_probability);
    }

    /// Assert an f64 is exactly `+0.0` (bit-exact, avoiding the float-cmp lint).
    fn assert_zero(value: f64) {
        assert_eq!(
            value.to_bits(),
            0.0_f64.to_bits(),
            "expected 0.0, got {value}"
        );
    }
}

#[cfg(test)]
mod buggify_knob_tests {
    use super::StorageConfiguration;
    use crate::chaos::{buggify_init, buggify_reset};
    use crate::sim::rng::{reset_sim_rng, set_config_seed, set_sim_seed};

    /// Sample the buggify-spiked knobs the way the runner does: seed both RNG
    /// streams and enable buggify before perturbing. Returns the spiked knobs
    /// (f64/`Duration` knobs as bits/nanos for exact comparison).
    fn buggified_knobs(seed: u64) -> [u64; 5] {
        reset_sim_rng();
        set_sim_seed(seed);
        set_config_seed(seed);
        buggify_init(0.8, 0.8);
        let mut config = StorageConfiguration::swarm_for_seed();
        config.apply_buggify_knobs();
        buggify_reset();
        [
            config.iops,
            config.bandwidth,
            config.sync_failure_probability.to_bits(),
            config.disk_stall_probability.to_bits(),
            u64::try_from(config.disk_throttle_duration.as_nanos()).unwrap_or(u64::MAX),
        ]
    }

    #[test]
    fn buggify_knobs_deterministic_per_seed() {
        for seed in [0_u64, 1, 42, 12_345] {
            assert_eq!(
                buggified_knobs(seed),
                buggified_knobs(seed),
                "buggify knob spikes must be reproducible for seed {seed}"
            );
        }
    }

    #[test]
    fn buggify_knobs_vary_across_seeds() {
        let distinct: std::collections::HashSet<_> = (0..50_u64).map(buggified_knobs).collect();
        assert!(
            distinct.len() > 1,
            "buggify knob spikes should vary across seeds"
        );
    }
}
