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
//! | Read EIO | `read_eio_probability` | 0% | The device refusing a read |
//! | Write EIO | `write_eio_probability` | 0% | The device refusing a write |
//! | Read corruption | `read_corruption_probability` | 0% | ECC failures, DRAM bit flips, media degradation |
//! | Write corruption | `write_corruption_probability` | 0% | Bad sectors, controller bugs |
//! | Crash damage | `crash_lost_probability` / `crash_latent_fault_probability` | 0% | Power loss damaging an unsynced sector |
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
//! ## Disk Failure
//!
//! A disk that *fails* is not slow, it is gone: every I/O issued to it after
//! the failure is accepted and never completes. FDB ref: `failedDisk` /
//! `waitUntilDiskReady()` returning `Never()`. Off by default.
//!
//! | Fault | Config Field | Default | Effect while active |
//! |-------|--------------|---------|---------------------|
//! | Disk failure | `disk_failure_probability` | 0% | Every later read/write/sync/`set_len` on that process's disk stays pending forever |
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
use crate::sim::rng::{sim_random_bool, sim_random_range};
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
    /// Per-sector probability that a read plants a *latent corruption*
    /// (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// ECC failures, DRAM bit flips, and media degradation: the read succeeds
    /// and returns bytes that are not the ones written. The damage is latent —
    /// it stays on the sector, identically, until the sector is rewritten — so
    /// a retry never heals it.
    pub read_corruption_probability: f64,

    /// Per-sector probability that a write plants a *latent corruption*
    /// (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Bad sectors and controller bugs: the write is acknowledged, but what
    /// lands is not what was sent. Caught only by an end-to-end checksum.
    pub write_corruption_probability: f64,

    /// Probability that a read fails outright with an I/O error (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// EIO is an *operating condition* — the device reporting that it could
    /// not serve the request — and is a different thing from a read that
    /// succeeds and returns corrupt bytes. Code has to handle both, and
    /// conflating them hides half the bugs, so they are separate families
    /// here: this one errors, `read_corruption_probability` damages.
    pub read_eio_probability: f64,

    /// Probability that a write fails outright with an I/O error (0.0 - 1.0).
    ///
    /// The write half of [`read_eio_probability`](Self::read_eio_probability):
    /// the bytes never reach the disk and the caller is told so, as opposed to
    /// a phantom write, which lies.
    pub write_eio_probability: f64,

    // =========================================================================
    // Crash Model
    // =========================================================================
    // What a crash *does* to the writes a sync had not yet made durable. These
    // are not "should a crash happen" knobs — the harness decides that — but
    // the physics a crash resolves with once one does. Every sector written
    // since the last sync resolves independently: kept old, kept new, lost,
    // damaged, or (opt-in) shorn.
    /// Probability that a crash is fully clean: every unsynced write survives
    /// intact. `FoundationDB`'s `AsyncFileNonDurable` uses `p = 0.1`.
    pub clean_crash_probability: f64,

    /// Probability that a (non-clean) crash rolls back a contiguous run of
    /// sectors together — correlated erase-block damage (Zheng, FAST'13).
    pub correlated_rollback_probability: f64,

    /// Maximum sector-run length for a correlated rollback.
    pub correlated_rollback_max_run: u64,

    /// Per-sector probability that an unsynced sector resolves as *lost* on
    /// crash: it reverts to never-written and reads the file's fill pattern.
    pub crash_lost_probability: f64,

    /// Per-sector probability that an unsynced sector resolves with a *latent
    /// read fault* on crash: the new contents land, but reads return
    /// deterministically corrupted bytes.
    pub crash_latent_fault_probability: f64,

    /// Per-sector probability that an unsynced sector is *shorn* on crash: a
    /// sub-sector prefix/suffix mix of old and new bytes.
    ///
    /// **Off by default**: enabling it deliberately weakens sector atomicity
    /// to model pre-`AWUPF` drives and RAID-split shorn writes (Zheng,
    /// FAST'13).
    pub shorn_write_probability: f64,

    /// Probability that an unsynced length change (a write past the end, a
    /// `set_len`) survives a crash. Otherwise the file reverts to its last
    /// durable length.
    pub length_survives_crash_probability: f64,

    /// Probability that a file's never-written and lost sectors read
    /// deterministic garbage rather than zeros, drawn once per file.
    ///
    /// Zeros are the dangerous real-world case (SATA `RZAT`, `NVMe` `DLFEAT`,
    /// unwritten extents): code that infers "never written" from "reads as
    /// zero" is wrong, and only a fill that is sometimes zero and sometimes
    /// garbage catches it.
    pub garbage_fill_probability: f64,

    /// Per-sector probability that a sync *lies*: it reports a sector durable
    /// while leaving it volatile, so a later crash loses or reorders a synced
    /// write (the fsyncgate class; Zheng's unserializable writes).
    ///
    /// **Off by default**, and it changes what the simulator considers a bug:
    /// with the family armed, a sector that changed across a crash after being
    /// reported durable is reported as a
    /// [`StorageFaultKind::LostSyncedWrite`](crate::storage::StorageFaultKind::LostSyncedWrite)
    /// instead of failing the run as a simulator bug.
    pub barrier_violation_probability: f64,

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
    // Device Geometry
    // =========================================================================
    /// Alignment a direct-I/O file on this disk demands of offsets, transfer
    /// lengths, and buffer addresses.
    ///
    /// Reported to callers through
    /// [`StorageFile::constraints`](moonpool_core::StorageFile::constraints)
    /// and enforced on every transfer, so code that would earn `EINVAL` from a
    /// real `O_DIRECT` file earns it here. Must be a power of two. This is a
    /// *device* property: it is not the caller's block or page size, and it
    /// says nothing about crash atomicity.
    pub direct_io_alignment: usize,

    /// Whether this disk can provide direct I/O at all.
    ///
    /// `false` models a filesystem that refuses `O_DIRECT` (tmpfs, several
    /// network filesystems): a
    /// [`DirectIo::Optional`](moonpool_core::DirectIo::Optional) open falls
    /// back to buffered I/O and a
    /// [`DirectIo::Required`](moonpool_core::DirectIo::Required) open fails.
    pub direct_io_supported: bool,

    /// Probability that one unsynced directory entry does not survive a crash
    /// (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// A create, delete, or rename changes the directory immediately but is
    /// only durable once the directory itself is synced
    /// ([`StorageProvider::sync_dir`](moonpool_core::StorageProvider::sync_dir)).
    /// A crash before that may leave a freshly created file nameless, or bring
    /// a deleted one back, however thoroughly the file's *contents* were
    /// synced. This is the fault that catches an engine which fsyncs its
    /// journal and forgets the directory holding it.
    ///
    /// The coin is drawn once per divergence between the visible namespace and
    /// the durable one, at crash time. While `0.0` (the default) a crash draws
    /// nothing here and the whole visible namespace survives.
    pub unsynced_dir_entry_loss_probability: f64,

    /// Per-operation probability that a read or write moves *fewer* bytes than
    /// asked for (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// `read(2)` and `write(2)` are allowed to transfer a prefix and return
    /// the count; so are their positioned forms. Callers that treat a
    /// `read_at`/`write_at` return value as "all of it" are wrong on real
    /// systems, and this knob makes them wrong here too, deterministically.
    ///
    /// A transfer is shortened to a non-empty prefix, never to zero: zero
    /// bytes means end of file for a read, and the caller would be right to
    /// stop. While `0.0` (the default) no operation draws from the RNG stream.
    pub short_transfer_probability: f64,

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

    // =========================================================================
    // Disk Failure
    // =========================================================================
    /// Per-operation probability that the owning process's disk *fails*
    /// (0.0 - 1.0).
    ///
    /// A failed disk answers nothing: the operation that failed it and every
    /// read, write, sync, or `set_len` issued to that process's files afterwards
    /// is accepted and never completes — the future stays `Pending` for the
    /// rest of the run. Nothing errors, nothing times out on the disk's behalf;
    /// only the caller's own timeout (or the process being killed) unblocks it.
    /// This is `FoundationDB`'s `failedDisk`, where `waitUntilDiskReady()`
    /// returns `Never()`.
    ///
    /// Scope and floor: the failure is keyed by the owning process IP, like a
    /// degradation episode, and **at most one disk is failed at a time** across
    /// the whole simulation, so a quorum system never loses more than one
    /// member to a hung disk. A crash or wipe of the owning process clears it
    /// (the reboot is the disk replacement) and fails the parked operations
    /// with `OperationInterrupted`; the next process to draw the coin may then
    /// fail its own disk. Recovery mode stops new failures but keeps one
    /// already in force.
    ///
    /// While `0.0` (the default) no operation draws from the RNG stream.
    pub disk_failure_probability: f64,
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
            read_corruption_probability: 0.0,
            write_corruption_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
            unsynced_dir_entry_loss_probability: 0.0,
            direct_io_alignment: 4096,
            direct_io_supported: true,
            short_transfer_probability: 0.0,
            read_eio_probability: 0.0,
            write_eio_probability: 0.0,
            clean_crash_probability: 0.1,
            correlated_rollback_probability: 0.25,
            correlated_rollback_max_run: 8,
            crash_lost_probability: 0.0,
            crash_latent_fault_probability: 0.0,
            shorn_write_probability: 0.0,
            length_survives_crash_probability: 0.5,
            garbage_fill_probability: 1.0,
            barrier_violation_probability: 0.0,

            // Dynamic disk degradation - disabled by default (no-op multipliers)
            disk_stall_probability: 0.0,
            disk_stall_duration: Duration::ZERO,
            disk_throttle_probability: 0.0,
            disk_throttle_duration: Duration::ZERO,
            disk_throttle_iops_multiplier: 1.0,
            disk_throttle_bandwidth_multiplier: 1.0,

            // Disk failure - disabled by default
            disk_failure_probability: 0.0,
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
            read_corruption_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            write_corruption_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            read_eio_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            write_eio_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            clean_crash_probability: 0.1,
            correlated_rollback_probability: 0.25,
            correlated_rollback_max_run: 8,
            crash_lost_probability: f64::from(sim_random_range(0..1000)) / 10_000.0,
            crash_latent_fault_probability: f64::from(sim_random_range(0..500)) / 10_000.0,
            shorn_write_probability: 0.0,
            length_survives_crash_probability: 0.5,
            garbage_fill_probability: 0.5,
            barrier_violation_probability: 0.0,
            misdirect_write_probability: f64::from(sim_random_range(0..10)) / 100_000.0,
            misdirect_read_probability: f64::from(sim_random_range(0..10)) / 100_000.0,
            phantom_write_probability: f64::from(sim_random_range(0..20)) / 100_000.0,
            sync_failure_probability: f64::from(sim_random_range(0..50)) / 100_000.0,
            unsynced_dir_entry_loss_probability: f64::from(sim_random_range(0..500)) / 100_000.0,
            direct_io_alignment: 4096,
            direct_io_supported: true,
            short_transfer_probability: f64::from(sim_random_range(0..200)) / 100_000.0,

            // Low-rate disk-degradation episodes (drawn after the faults so the
            // existing per-field RNG sub-sequence is unchanged).
            disk_stall_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            disk_stall_duration: Duration::from_millis(sim_random_range(50..500)),
            disk_throttle_probability: f64::from(sim_random_range(0..100)) / 100_000.0,
            disk_throttle_duration: Duration::from_millis(sim_random_range(500..5000)),
            disk_throttle_iops_multiplier: f64::from(sim_random_range(2..20)),
            disk_throttle_bandwidth_multiplier: f64::from(sim_random_range(2..20)),

            // Disk failure: 0 to 0.01% per operation, drawn last so every field
            // above keeps its per-seed value.
            disk_failure_probability: f64::from(sim_random_range(0..10)) / 100_000.0,
        }
    }

    /// Create a swarm-testing storage configuration for seed-based testing.
    ///
    /// Starts from [`random_for_seed`](Self::random_for_seed), then disables each
    /// fault family with ~50% probability (drawn from the simulation stream).
    /// This implements *swarm testing* (Groce et al., ISSTA 2012): each
    /// seed exercises a random *subset* of storage fault families — including the
    /// all-off subset — instead of every family being slightly on at once (which
    /// lets families crowd each other out, the passive-suppression anti-pattern).
    #[must_use]
    pub fn swarm_for_seed() -> Self {
        let mut config = Self::random_for_seed();
        config.apply_swarm_mask();
        config
    }

    /// Disable each fault family with ~50% probability using the simulation
    /// stream (see [`swarm_for_seed`](Self::swarm_for_seed)).
    ///
    /// Draws exactly ten `sim_random_bool` values (one per family: the seven
    /// per-op faults, the stall and throttle episode families, and the disk
    /// failure) so the call sequence is fixed and reproducible per
    /// seed. Performance
    /// parameters (IOPS, bandwidth, latencies) stay as sampled — only the fault
    /// families are masked.
    fn apply_swarm_mask(&mut self) {
        if !sim_random_bool(0.5) {
            self.read_corruption_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.write_corruption_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.crash_lost_probability = 0.0;
            self.crash_latent_fault_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.misdirect_read_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.misdirect_write_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.phantom_write_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.sync_failure_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.disk_stall_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.disk_throttle_probability = 0.0;
        }
        // Appended last: keeps the nine draws above stable across seeds.
        if !sim_random_bool(0.5) {
            self.disk_failure_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.short_transfer_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.unsynced_dir_entry_loss_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.read_eio_probability = 0.0;
        }
        if !sim_random_bool(0.5) {
            self.write_eio_probability = 0.0;
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
        // A failed disk is bounded by count (one at a time), not by rate, so a
        // spiked rate only moves the failure earlier in the run.
        self.disk_failure_probability =
            crate::buggify_knob!(self.disk_failure_probability, 0.001..0.01);
    }

    /// Turn every storage fault family off, leaving disk performance alone.
    ///
    /// This is the storage half of the recovery-mode transition
    /// ([`SimWorld::enter_recovery_mode`](crate::SimWorld::enter_recovery_mode)):
    /// after this call no operation samples a read/write/sync/crash fault, no
    /// write is misdirected or turned into a phantom, no new disk stall or
    /// throttle episode is entered, and no disk fails. It consumes no
    /// randomness.
    ///
    /// It only stops *new* faults. Sectors already corrupted stay corrupted,
    /// bytes already lost stay lost, misdirected and phantom writes already
    /// applied are not undone, an episode already in force runs to its
    /// expiry, and a disk that already failed stays failed until its process
    /// is crashed or wiped.
    ///
    /// IOPS, bandwidth, and the read/write/sync latency distributions are
    /// untouched — they are the disk's normal characteristics. So are the
    /// throttle multipliers, which an episode still ticking down needs in order
    /// to keep behaving the way it did before the cutoff.
    pub fn disable_fault_injection(&mut self) {
        self.read_corruption_probability = 0.0;
        self.write_corruption_probability = 0.0;
        self.barrier_violation_probability = 0.0;
        self.read_eio_probability = 0.0;
        self.write_eio_probability = 0.0;
        self.misdirect_write_probability = 0.0;
        self.misdirect_read_probability = 0.0;
        self.phantom_write_probability = 0.0;
        self.sync_failure_probability = 0.0;
        self.short_transfer_probability = 0.0;
        self.unsynced_dir_entry_loss_probability = 0.0;
        self.disk_stall_probability = 0.0;
        self.disk_throttle_probability = 0.0;
        self.disk_failure_probability = 0.0;
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
            read_corruption_probability: 0.0,
            write_corruption_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
            unsynced_dir_entry_loss_probability: 0.0,
            direct_io_alignment: 4096,
            direct_io_supported: true,
            short_transfer_probability: 0.0,
            read_eio_probability: 0.0,
            write_eio_probability: 0.0,

            // Crashes still resolve unsynced writes, but never damage a
            // sector: it lands old or new and nothing else.
            clean_crash_probability: 0.1,
            correlated_rollback_probability: 0.25,
            correlated_rollback_max_run: 8,
            crash_lost_probability: 0.0,
            crash_latent_fault_probability: 0.0,
            shorn_write_probability: 0.0,
            length_survives_crash_probability: 0.5,
            garbage_fill_probability: 1.0,
            barrier_violation_probability: 0.0,

            // Disk degradation disabled (no-op multipliers)
            disk_stall_probability: 0.0,
            disk_stall_duration: Duration::ZERO,
            disk_throttle_probability: 0.0,
            disk_throttle_duration: Duration::ZERO,
            disk_throttle_iops_multiplier: 1.0,
            disk_throttle_bandwidth_multiplier: 1.0,

            // Disk failure disabled
            disk_failure_probability: 0.0,
        }
    }
}

#[cfg(test)]
mod swarm_tests {
    use super::StorageConfiguration;
    use crate::sim::rng::{reset_sim_rng, set_sim_seed};

    /// The on/off state of each swarmed fault family, in mask order.
    fn enabled_families(config: &StorageConfiguration) -> [bool; FAMILY_COUNT as usize] {
        [
            config.read_corruption_probability > 0.0,
            config.write_corruption_probability > 0.0,
            config.crash_lost_probability > 0.0 || config.crash_latent_fault_probability > 0.0,
            config.misdirect_read_probability > 0.0,
            config.misdirect_write_probability > 0.0,
            config.phantom_write_probability > 0.0,
            config.sync_failure_probability > 0.0,
            config.disk_stall_probability > 0.0,
            config.disk_throttle_probability > 0.0,
            config.disk_failure_probability > 0.0,
            config.short_transfer_probability > 0.0,
            config.unsynced_dir_entry_loss_probability > 0.0,
            config.read_eio_probability > 0.0,
            config.write_eio_probability > 0.0,
        ]
    }

    /// How many seeds the reachability tests scan.
    ///
    /// The all-off subset needs every family's coin to come up off at once, so
    /// the seeds needed grow as `2^families`. Scale the scan with the family
    /// count rather than pinning a number that silently stops covering the
    /// case as families are added.
    const REACHABILITY_SEEDS: u64 = 1 << (FAMILY_COUNT + 3);

    /// Number of fault families the swarm mask covers.
    const FAMILY_COUNT: u32 = 14;

    /// Build a swarm config the way the runner does: the stream seeded per iteration.
    fn swarm_for(seed: u64) -> StorageConfiguration {
        reset_sim_rng();
        set_sim_seed(seed);
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

        for seed in 0..REACHABILITY_SEEDS {
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
            "no seed below {REACHABILITY_SEEDS} produced the all-off subset"
        );
        assert!(
            saw_mixed,
            "no seed below {REACHABILITY_SEEDS} produced a mixed subset"
        );
    }

    #[test]
    fn swarm_all_off_seed_has_zero_fault_probabilities() {
        // Find a seed whose subset is entirely off, then assert every family is inert.
        let seed = (0..REACHABILITY_SEEDS)
            .find(|&s| enabled_families(&swarm_for(s)).iter().all(|&e| !e))
            .expect("expected an all-off seed within the scanned range");

        let config = swarm_for(seed);
        assert_zero(config.read_corruption_probability);
        assert_zero(config.write_corruption_probability);
        assert_zero(config.crash_lost_probability);
        assert_zero(config.crash_latent_fault_probability);
        assert_zero(config.misdirect_read_probability);
        assert_zero(config.misdirect_write_probability);
        assert_zero(config.phantom_write_probability);
        assert_zero(config.sync_failure_probability);
        assert_zero(config.disk_stall_probability);
        assert_zero(config.disk_throttle_probability);
        assert_zero(config.disk_failure_probability);
        assert_zero(config.short_transfer_probability);
        assert_zero(config.unsynced_dir_entry_loss_probability);
        assert_zero(config.read_eio_probability);
        assert_zero(config.write_eio_probability);
    }

    #[test]
    fn disable_fault_injection_turns_off_the_disk_failure_family() {
        let mut config = StorageConfiguration {
            disk_failure_probability: 1.0,
            ..StorageConfiguration::fast_local()
        };
        config.disable_fault_injection();
        assert_zero(config.disk_failure_probability);
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
    use crate::sim::rng::{reset_sim_rng, set_sim_seed};

    /// Sample the buggify-spiked knobs the way the runner does: seed the
    /// stream and enable buggify before perturbing. Returns the spiked knobs
    /// (f64/`Duration` knobs as bits/nanos for exact comparison).
    fn buggified_knobs(seed: u64) -> [u64; 5] {
        reset_sim_rng();
        set_sim_seed(seed);
        buggify_init(0.8);
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
        let distinct: std::collections::BTreeSet<_> = (0..50_u64).map(buggified_knobs).collect();
        assert!(
            distinct.len() > 1,
            "buggify knob spikes should vary across seeds"
        );
    }
}
