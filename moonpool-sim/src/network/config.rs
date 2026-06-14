//! # Network Chaos Configuration
//!
//! This module provides configuration for network chaos testing, following
//! FoundationDB's battle-tested simulation approach and TigerBeetle's deterministic
//! testing patterns.
//!
//! ## Connection Failure Modes
//!
//! | Failure | Config Field | Default | Real-World Scenario |
//! |---------|--------------|---------|---------------------|
//! | Random close | `random_close_probability` | 0.001% | Reconnection logic, message redelivery, connection pooling |
//! | Asymmetric close | `random_close_explicit_ratio` | 30% explicit | Half-closed sockets, FIN vs RST handling |
//! | Connect failure | `connect_failure_mode` | Probabilistic | Connection establishment retries, timeout handling |
//!
//! ## Network Latency & Congestion
//!
//! | Delay Type | Config Field | Default | Real-World Scenario |
//! |------------|--------------|---------|---------------------|
//! | Operation latency | `bind/accept/connect/read/write_latency` | Various ranges | Timeout settings, async operation ordering |
//! | Write clogging | `clog_probability` + `clog_duration` | 0%, 100-300ms | Backpressure handling, flow control |
//! | Read clogging | Same as write | Same | Symmetric flow control |
//! | Clock drift | `clock_drift_enabled` + `clock_drift_max` | true, 100ms | Lease expiration, distributed consensus, TTL handling |
//! | Buggified delay | `buggified_delay_enabled` + `buggified_delay_max` | true, 100ms | Race conditions, timing-dependent bugs |
//!
//! ## Network Partitions
//!
//! | Partition Type | Config/Method | Default | Real-World Scenario |
//! |----------------|---------------|---------|---------------------|
//! | Random partition | `partition_probability` + `partition_duration` | 0%, 200ms-2s | Split-brain, quorum loss, leader election |
//! | Bi-directional | `partition_pair()` | Manual | Complete isolation between nodes |
//! | Send-only block | `partition_send_from()` | Manual | Asymmetric network failures |
//! | Recv-only block | `partition_recv_to()` | Manual | Asymmetric network failures |
//! | Partition strategy | `partition_strategy` | Random | Different failure patterns (uniform, isolate) |
//!
//! ## Data Integrity Faults
//!
//! | Fault | Config Field | Default | Real-World Scenario |
//! |-------|--------------|---------|---------------------|
//! | Bit flip | `bit_flip_probability` + `bit_flip_min/max_bits` | 0.01%, 1-32 bits | CRC/checksum validation, data corruption detection |
//!
//! ## Partial Write Simulation
//!
//! | Feature | Config Field | Default | Real-World Scenario |
//! |---------|--------------|---------|---------------------|
//! | Short writes | `partial_write_max_bytes` | 1000 bytes | TCP fragmentation handling, message framing |
//!
//! ## Configuration Examples
//!
//! ### Fast Local Testing (No Chaos)
//! ```rust
//! use moonpool_sim::network::{NetworkConfiguration, ChaosConfiguration};
//!
//! let config = NetworkConfiguration::fast_local();
//! // All chaos disabled, minimal latencies
//! ```
//!
//! ### Full Chaos Testing
//! ```rust
//! use moonpool_sim::network::{NetworkConfiguration, ChaosConfiguration};
//!
//! let config = NetworkConfiguration::random_for_seed();
//! // Randomized chaos parameters for comprehensive testing
//! ```
//!
//! ### Custom Configuration
//! ```rust
//! use moonpool_sim::network::{NetworkConfiguration, PartitionStrategy};
//!
//! let mut config = NetworkConfiguration::default();
//! config.chaos.partition_strategy = PartitionStrategy::IsolateSingle;
//! config.chaos.partition_probability = 0.05; // 5%
//! ```
//!
//! ## FDB/TigerBeetle References
//!
//! - Random close: FDB sim2.actor.cpp:580-605
//! - Partitions: FDB SimClogging, TigerBeetle partition modes
//! - Bit flips: FDB FlowTransport.actor.cpp:1297
//! - Clock drift: FDB sim2.actor.cpp:1058-1064
//! - Connect failures: FDB sim2.actor.cpp:1243-1250

use crate::sim::rng::{config_random_bool, sim_random_range, sim_random_range_or_default};
use std::ops::Range;
use std::time::Duration;

/// Network partition strategy for chaos testing.
///
/// Controls how nodes are selected for partitioning during chaos testing.
/// `TigerBeetle` ref: packet_simulator.zig:12-488
///
/// # Real-World Scenario
///
/// Different partition strategies test different failure modes:
/// - Random: General chaos, unpredictable failures
/// - `UniformSize`: Tests various quorum sizes and split scenarios
/// - `IsolateSingle`: Tests single-node isolation (common in production)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionStrategy {
    /// Random IP pairs selected for partitioning.
    /// Current behavior - randomly selects which connections to partition.
    #[default]
    Random,

    /// Uniform size partitions - randomly choose partition size from 1 to n-1 nodes.
    /// `TigerBeetle` pattern: creates partitions of varying sizes to test different
    /// quorum scenarios.
    UniformSize,

    /// Isolate single node - always partition exactly one node from the rest.
    /// Tests the common production scenario where a single node becomes unreachable.
    IsolateSingle,
}

/// Connection establishment failure mode for fault injection.
///
/// Controls how connection attempts fail during chaos testing.
/// FDB ref: sim2.actor.cpp:1243-1250 (`SIM_CONNECT_ERROR_MODE`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectFailureMode {
    /// Disabled - no connection failures injected
    #[default]
    Disabled,
    /// Always fail with `ConnectionRefused` when buggified
    AlwaysFail,
    /// Probabilistic: 50% fail with `ConnectionRefused`, 50% hang forever
    Probabilistic,
}

impl ConnectFailureMode {
    /// Create a random failure mode for chaos testing
    #[must_use]
    pub fn random_for_seed() -> Self {
        match sim_random_range(0..3) {
            0 => Self::Disabled,
            1 => Self::AlwaysFail,
            _ => Self::Probabilistic,
        }
    }
}

/// Configuration for chaos injection in simulations.
///
/// This struct contains all settings related to fault injection and chaos testing,
/// following `FoundationDB`'s BUGGIFY patterns for deterministic testing.
#[derive(Debug, Clone)]
pub struct ChaosConfiguration {
    /// Clogging probability for individual writes (0.0 - 1.0)
    pub clog_probability: f64,
    /// Duration range for clog delays
    pub clog_duration: Range<Duration>,

    /// Network partition probability (0.0 - 1.0)
    pub partition_probability: f64,
    /// Duration range for network partitions
    pub partition_duration: Range<Duration>,

    /// Bit flip probability for packet corruption (0.0 - 1.0)
    pub bit_flip_probability: f64,
    /// Minimum number of bits to flip (power-law distribution lower bound)
    pub bit_flip_min_bits: u32,
    /// Maximum number of bits to flip (power-law distribution upper bound)
    pub bit_flip_max_bits: u32,
    /// Cooldown duration after bit flip to prevent excessive corruption
    pub bit_flip_cooldown: Duration,

    /// Maximum bytes for partial write simulation (BUGGIFY truncates writes to 0-max_bytes)
    /// Following FDB's approach of truncating writes to test TCP backpressure handling
    pub partial_write_max_bytes: usize,

    /// Random connection close probability per I/O operation (0.0 - 1.0)
    /// FDB default: 0.00001 (0.001%) - see sim2.actor.cpp:584
    pub random_close_probability: f64,

    /// Cooldown duration after a random close event (prevents cascading failures)
    /// FDB uses connectionFailuresDisableDuration - see sim2.actor.cpp:583
    pub random_close_cooldown: Duration,

    /// Ratio of explicit exceptions vs silent failures (0.0 - 1.0)
    /// FDB default: 0.3 (30% explicit) - see sim2.actor.cpp:602
    pub random_close_explicit_ratio: f64,

    /// Enable clock drift simulation
    /// When enabled, `timer()` can return a time up to `clock_drift_max` ahead of `now()`
    /// FDB ref: sim2.actor.cpp:1058-1064
    pub clock_drift_enabled: bool,

    /// Maximum clock drift (default 100ms per FDB)
    /// `timer()` can be up to this much ahead of `now()`
    pub clock_drift_max: Duration,

    /// Enable buggified delays on sleep/timer operations
    /// When enabled, 25% of sleep operations get extra delay
    /// FDB ref: sim2.actor.cpp:1100-1105
    pub buggified_delay_enabled: bool,

    /// Maximum additional delay for buggified sleep (default 100ms)
    /// Uses power-law distribution: `max_delay` * `pow(random01()`, 1000.0)
    /// FDB ref: sim2.actor.cpp:1104
    pub buggified_delay_max: Duration,

    /// Probability of adding buggified delay (default 25% per FDB)
    pub buggified_delay_probability: f64,

    /// Connection establishment failure mode (per FDB)
    /// FDB ref: sim2.actor.cpp:1243-1250 (`SIM_CONNECT_ERROR_MODE`)
    pub connect_failure_mode: ConnectFailureMode,

    /// Probability of connect failure when Probabilistic mode is enabled (default 50%)
    pub connect_failure_probability: f64,

    /// Permanent per-IP-pair latency range (FDB `SimClogging::MAX_CLOGGING_LATENCY`).
    /// Each ordered IP pair samples a fixed latency from this range at first contact
    /// (via [`sample_duration`]), then adds it to every delivery on that pair for the
    /// whole run — modelling a stably-slow link. An all-zero range (`end` is zero)
    /// disables it, leaving behavior unchanged. FDB's `MAX * random01()` is the
    /// `ZERO..MAX` case. FDB ref: `sim2.actor.cpp` ~294-299, 352-354.
    pub max_pair_latency: Range<Duration>,

    /// Network partition strategy.
    /// Controls how nodes are selected for partitioning.
    /// `TigerBeetle` ref: `packet_simulator.zig` partition modes
    ///
    /// # Real-World Scenario
    /// Different strategies test different failure scenarios:
    /// - Random: unpredictable chaos
    /// - `UniformSize`: various quorum sizes
    /// - `IsolateSingle`: single node isolation (common in production)
    pub partition_strategy: PartitionStrategy,
}

impl Default for ChaosConfiguration {
    fn default() -> Self {
        Self {
            clog_probability: 0.0,
            clog_duration: Duration::from_millis(100)..Duration::from_millis(300),
            partition_probability: 0.0,
            partition_duration: Duration::from_millis(200)..Duration::from_secs(2),
            bit_flip_probability: 0.0001, // 0.01% - matches FDB's BUGGIFY_WITH_PROB(0.0001)
            bit_flip_min_bits: 1,
            bit_flip_max_bits: 32,
            bit_flip_cooldown: Duration::ZERO, // No cooldown by default for maximum chaos
            partial_write_max_bytes: 1000,     // Matches FDB's randomInt(0, 1000)
            random_close_probability: 0.00001, // 0.001% - matches FDB's sim2.actor.cpp:584
            random_close_cooldown: Duration::from_secs(5), // Reasonable default
            random_close_explicit_ratio: 0.3,  // 30% explicit - matches FDB's sim2.actor.cpp:602
            clock_drift_enabled: true,         // Enable by default for chaos testing
            clock_drift_max: Duration::from_millis(100), // FDB default: 0.1 seconds
            buggified_delay_enabled: true,     // Enable by default for chaos testing
            buggified_delay_max: Duration::from_millis(100), // FDB: MAX_BUGGIFIED_DELAY
            buggified_delay_probability: 0.25, // FDB: random01() < 0.25
            connect_failure_mode: ConnectFailureMode::Probabilistic, // FDB: SIM_CONNECT_ERROR_MODE = 2
            connect_failure_probability: 0.5,                        // FDB: random01() > 0.5
            max_pair_latency: Duration::ZERO..Duration::ZERO, // FDB: MAX_CLOGGING_LATENCY default 0
            partition_strategy: PartitionStrategy::default(),
        }
    }
}

impl ChaosConfiguration {
    /// Create a configuration with all chaos disabled (for fast local testing)
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            clog_probability: 0.0,
            clog_duration: Duration::ZERO..Duration::ZERO,
            partition_probability: 0.0,
            partition_duration: Duration::ZERO..Duration::ZERO,
            bit_flip_probability: 0.0,
            bit_flip_min_bits: 1,
            bit_flip_max_bits: 32,
            bit_flip_cooldown: Duration::ZERO,
            partial_write_max_bytes: 1000,
            random_close_probability: 0.0,
            random_close_cooldown: Duration::ZERO,
            random_close_explicit_ratio: 0.3,
            clock_drift_enabled: false,
            clock_drift_max: Duration::from_millis(100),
            buggified_delay_enabled: false,
            buggified_delay_max: Duration::from_millis(100),
            buggified_delay_probability: 0.25,
            connect_failure_mode: ConnectFailureMode::Disabled,
            connect_failure_probability: 0.5,
            max_pair_latency: Duration::ZERO..Duration::ZERO,
            partition_strategy: PartitionStrategy::Random, // Default strategy
        }
    }

    /// Create a randomized chaos configuration for seed-based testing
    #[must_use]
    pub fn random_for_seed() -> Self {
        Self {
            clog_probability: f64::from(sim_random_range(0..20)) / 100.0, // 0-20% for clogging
            clog_duration: Duration::from_micros(sim_random_range(50_000..300_000))
                ..Duration::from_micros(sim_random_range(100_000..500_000)),
            partition_probability: f64::from(sim_random_range(0..15)) / 100.0, // 0-15% (lower than faults)
            partition_duration: Duration::from_millis(sim_random_range(100..1000))
                ..Duration::from_millis(sim_random_range(500..3000)),
            // Bit flip probability range: 0.001% to 0.02% (very low, like FDB)
            bit_flip_probability: f64::from(sim_random_range(1..20)) / 100_000.0,
            bit_flip_min_bits: 1,
            bit_flip_max_bits: 32,
            bit_flip_cooldown: Duration::from_millis(sim_random_range(0..100)),
            partial_write_max_bytes: sim_random_range(100..2000), // Vary max bytes for different scenarios
            // Random close probability: 0.0001% to 0.01% (very low, like FDB)
            random_close_probability: f64::from(sim_random_range(1..100)) / 1_000_000.0,
            random_close_cooldown: Duration::from_millis(sim_random_range(1000..10_000)),
            random_close_explicit_ratio: f64::from(sim_random_range(20..40)) / 100.0, // 20-40%
            clock_drift_enabled: true,
            clock_drift_max: Duration::from_millis(sim_random_range(50..150)), // 50-150ms
            buggified_delay_enabled: true,
            buggified_delay_max: Duration::from_millis(sim_random_range(50..150)), // 50-150ms
            buggified_delay_probability: f64::from(sim_random_range(20..30)) / 100.0, // 20-30%
            connect_failure_mode: ConnectFailureMode::random_for_seed(),
            connect_failure_probability: f64::from(sim_random_range(40..60)) / 100.0, // 40-60%
            // Randomly choose partition strategy
            partition_strategy: match sim_random_range(0..3) {
                0 => PartitionStrategy::Random,
                1 => PartitionStrategy::UniformSize,
                _ => PartitionStrategy::IsolateSingle,
            },
            // Permanent per-pair latency, randomized upper bound up to 100ms (FDB
            // buggifies MAX_CLOGGING_LATENCY to 0.1s). Kept last so existing RNG
            // draws above are unaffected.
            max_pair_latency: Duration::ZERO..Duration::from_millis(sim_random_range(0..100)),
        }
    }

    /// Create a swarm-testing chaos configuration for seed-based testing.
    ///
    /// Starts from [`random_for_seed`](Self::random_for_seed), then disables each
    /// fault family with ~50% probability (drawn from the independent `CONFIG_RNG`
    /// stream). This implements *swarm testing* (Groce et al., ISSTA 2012): each
    /// seed exercises a random *subset* of fault families — including the all-off
    /// subset — instead of every family being slightly on at once (which lets
    /// families crowd each other out, the passive-suppression anti-pattern).
    #[must_use]
    pub fn swarm_for_seed() -> Self {
        let mut chaos = Self::random_for_seed();
        chaos.apply_swarm_mask();
        chaos
    }

    /// Disable each fault family with ~50% probability using the `CONFIG_RNG`
    /// stream (see [`swarm_for_seed`](Self::swarm_for_seed)).
    ///
    /// Draws exactly eight `config_random_bool` values (one per family) so the
    /// `CONFIG_RNG` call sequence is fixed and reproducible per seed. Durations,
    /// cooldowns, and strategy stay as sampled — they are inert once their family
    /// is off.
    fn apply_swarm_mask(&mut self) {
        if !config_random_bool(0.5) {
            self.clog_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.partition_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.bit_flip_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.random_close_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.connect_failure_mode = ConnectFailureMode::Disabled;
            self.connect_failure_probability = 0.0;
        }
        self.clock_drift_enabled = config_random_bool(0.5);
        self.buggified_delay_enabled = config_random_bool(0.5);
        // Appended last: keeps the seven draws above stable across seeds.
        if !config_random_bool(0.5) {
            self.max_pair_latency = Duration::ZERO..Duration::ZERO;
        }
    }
}

/// Configuration for network simulation parameters
#[derive(Debug, Clone)]
pub struct NetworkConfiguration {
    /// Latency range for bind operations
    pub bind_latency: Range<Duration>,
    /// Latency range for accept operations
    pub accept_latency: Range<Duration>,
    /// Latency range for connect operations
    pub connect_latency: Range<Duration>,
    /// Latency range for read operations
    pub read_latency: Range<Duration>,
    /// Latency range for write operations
    pub write_latency: Range<Duration>,

    /// Chaos injection configuration
    pub chaos: ChaosConfiguration,
}

impl Default for NetworkConfiguration {
    fn default() -> Self {
        Self {
            bind_latency: Duration::from_micros(50)..Duration::from_micros(150),
            accept_latency: Duration::from_millis(1)..Duration::from_millis(6),
            connect_latency: Duration::from_millis(1)..Duration::from_millis(11),
            read_latency: Duration::from_micros(10)..Duration::from_micros(60),
            write_latency: Duration::from_micros(100)..Duration::from_micros(600),
            chaos: ChaosConfiguration::default(),
        }
    }
}

/// Sample a random duration from a range
#[must_use]
pub fn sample_duration(range: &Range<Duration>) -> Duration {
    let start_nanos = u64::try_from(range.start.as_nanos()).unwrap_or(u64::MAX);
    let end_nanos = u64::try_from(range.end.as_nanos()).unwrap_or(u64::MAX);
    let random_nanos = sim_random_range_or_default(start_nanos..end_nanos);
    Duration::from_nanos(random_nanos)
}

impl NetworkConfiguration {
    /// Create a new network configuration with default settings
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a randomized network configuration for chaos testing
    #[must_use]
    pub fn random_for_seed() -> Self {
        Self {
            bind_latency: Duration::from_micros(sim_random_range(10..200))
                ..Duration::from_micros(sim_random_range(50..300)),
            accept_latency: Duration::from_micros(sim_random_range(1000..10_000))
                ..Duration::from_micros(sim_random_range(5000..15_000)),
            connect_latency: Duration::from_micros(sim_random_range(1000..50_000))
                ..Duration::from_micros(sim_random_range(10_000..100_000)),
            read_latency: Duration::from_micros(sim_random_range(5..100))
                ..Duration::from_micros(sim_random_range(50..200)),
            write_latency: Duration::from_micros(sim_random_range(50..1000))
                ..Duration::from_micros(sim_random_range(200..2000)),
            chaos: ChaosConfiguration::random_for_seed(),
        }
    }

    /// Create a swarm-testing network configuration for seed-based testing.
    ///
    /// Identical to [`random_for_seed`](Self::random_for_seed) for the baseline
    /// latencies, but the embedded [`ChaosConfiguration`] enables only a random
    /// *subset* of fault families per seed. See
    /// [`ChaosConfiguration::swarm_for_seed`].
    #[must_use]
    pub fn swarm_for_seed() -> Self {
        let mut config = Self::random_for_seed();
        config.chaos.apply_swarm_mask();
        config
    }

    /// Create a configuration optimized for fast local testing
    #[must_use]
    pub fn fast_local() -> Self {
        let one_us = Duration::from_micros(1);
        let ten_us = Duration::from_micros(10);
        Self {
            bind_latency: one_us..one_us,
            accept_latency: ten_us..ten_us,
            connect_latency: ten_us..ten_us,
            read_latency: one_us..one_us,
            write_latency: one_us..one_us,
            chaos: ChaosConfiguration::disabled(),
        }
    }
}

#[cfg(test)]
mod swarm_tests {
    use super::{ChaosConfiguration, ConnectFailureMode, NetworkConfiguration};
    use crate::sim::rng::{reset_sim_rng, set_config_seed, set_sim_seed};

    /// The on/off state of each swarmed fault family, in mask order.
    fn enabled_families(chaos: &ChaosConfiguration) -> [bool; 7] {
        [
            chaos.clog_probability > 0.0,
            chaos.partition_probability > 0.0,
            chaos.bit_flip_probability > 0.0,
            chaos.random_close_probability > 0.0,
            chaos.connect_failure_mode != ConnectFailureMode::Disabled,
            chaos.clock_drift_enabled,
            chaos.buggified_delay_enabled,
        ]
    }

    /// Build a swarm config the way the runner does: both streams seeded per iteration.
    fn swarm_for(seed: u64) -> NetworkConfiguration {
        reset_sim_rng();
        set_sim_seed(seed);
        set_config_seed(seed);
        NetworkConfiguration::swarm_for_seed()
    }

    #[test]
    fn swarm_subset_is_deterministic_per_seed() {
        for seed in [0_u64, 1, 42, 12_345] {
            let first = enabled_families(&swarm_for(seed).chaos);
            let second = enabled_families(&swarm_for(seed).chaos);
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
            let families = enabled_families(&swarm_for(seed).chaos);
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
            .find(|&s| enabled_families(&swarm_for(s).chaos).iter().all(|&e| !e))
            .expect("expected an all-off seed within 0..1000");

        let chaos = swarm_for(seed).chaos;
        assert_zero(chaos.clog_probability);
        assert_zero(chaos.partition_probability);
        assert_zero(chaos.bit_flip_probability);
        assert_zero(chaos.random_close_probability);
        assert_eq!(chaos.connect_failure_mode, ConnectFailureMode::Disabled);
        assert_zero(chaos.connect_failure_probability);
        assert!(!chaos.clock_drift_enabled);
        assert!(!chaos.buggified_delay_enabled);
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
