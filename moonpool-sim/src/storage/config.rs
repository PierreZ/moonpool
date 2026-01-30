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

use crate::sim::rng::sim_random_range;
use std::ops::Range;
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
    /// - NVMe SSD: 100,000-500,000 IOPS
    /// - SATA SSD: 25,000-100,000 IOPS
    /// - HDD: 100-200 IOPS
    pub iops: u64,

    /// Maximum bandwidth in bytes per second.
    ///
    /// Typical values:
    /// - NVMe SSD: 3,000-7,000 MB/s
    /// - SATA SSD: 500-600 MB/s
    /// - HDD: 100-200 MB/s
    pub bandwidth: u64,

    /// Latency range for read operations.
    ///
    /// Typical values:
    /// - NVMe SSD: 20-100µs
    /// - SATA SSD: 50-200µs
    /// - HDD: 2-10ms
    pub read_latency: Range<Duration>,

    /// Latency range for write operations.
    ///
    /// Typical values:
    /// - NVMe SSD: 20-100µs
    /// - SATA SSD: 100-500µs
    /// - HDD: 2-10ms
    pub write_latency: Range<Duration>,

    /// Latency range for sync/flush operations.
    ///
    /// Sync operations ensure data durability and typically take
    /// longer than regular read/write operations.
    pub sync_latency: Range<Duration>,

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
    /// TigerBeetle ref: storage fault injection
    pub misdirect_write_probability: f64,

    /// Probability of read returning data from wrong location (0.0 - 1.0).
    ///
    /// # Real-World Scenario
    /// Simulates misdirected reads where data is read from a different
    /// block than intended. Tests checksum validation.
    /// TigerBeetle ref: storage fault injection
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
}

impl Default for StorageConfiguration {
    fn default() -> Self {
        Self {
            // Performance parameters matching a typical SATA SSD
            iops: 25_000,
            bandwidth: 150_000_000, // 150 MB/s
            read_latency: Duration::from_micros(50)..Duration::from_micros(200),
            write_latency: Duration::from_micros(100)..Duration::from_micros(500),
            sync_latency: Duration::from_millis(1)..Duration::from_millis(5),

            // Fault probabilities - disabled by default for predictable behavior
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
        }
    }
}

impl StorageConfiguration {
    /// Create a new storage configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a randomized storage configuration for chaos testing.
    ///
    /// Uses deterministic random values based on the simulation seed
    /// to create varied configurations across test runs.
    pub fn random_for_seed() -> Self {
        Self {
            // Randomize IOPS between 10,000 and 100,000
            iops: sim_random_range(10_000..100_000),

            // Randomize bandwidth between 50 MB/s and 500 MB/s
            bandwidth: sim_random_range(50_000_000..500_000_000),

            // Randomize latencies
            read_latency: Duration::from_micros(sim_random_range(20..100))
                ..Duration::from_micros(sim_random_range(100..500)),
            write_latency: Duration::from_micros(sim_random_range(50..200))
                ..Duration::from_micros(sim_random_range(200..1000)),
            sync_latency: Duration::from_micros(sim_random_range(500..2000))
                ..Duration::from_micros(sim_random_range(2000..10000)),

            // Low fault probabilities for chaos testing (0.001% to 0.1%)
            read_fault_probability: sim_random_range(0..100) as f64 / 100_000.0,
            write_fault_probability: sim_random_range(0..100) as f64 / 100_000.0,
            crash_fault_probability: sim_random_range(0..50) as f64 / 100_000.0,
            misdirect_write_probability: sim_random_range(0..10) as f64 / 100_000.0,
            misdirect_read_probability: sim_random_range(0..10) as f64 / 100_000.0,
            phantom_write_probability: sim_random_range(0..20) as f64 / 100_000.0,
            sync_failure_probability: sim_random_range(0..50) as f64 / 100_000.0,
        }
    }

    /// Create a configuration optimized for fast local testing.
    ///
    /// Minimal latencies and no fault injection for predictable,
    /// fast test execution.
    pub fn fast_local() -> Self {
        let one_us = Duration::from_micros(1);
        Self {
            iops: 1_000_000,          // Very high IOPS
            bandwidth: 1_000_000_000, // 1 GB/s
            read_latency: one_us..one_us,
            write_latency: one_us..one_us,
            sync_latency: one_us..one_us,

            // All faults disabled
            read_fault_probability: 0.0,
            write_fault_probability: 0.0,
            crash_fault_probability: 0.0,
            misdirect_write_probability: 0.0,
            misdirect_read_probability: 0.0,
            phantom_write_probability: 0.0,
            sync_failure_probability: 0.0,
        }
    }
}
