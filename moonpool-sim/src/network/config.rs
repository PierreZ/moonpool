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
//! | Connection cut | Manual via `cut_connection()` | N/A | Temporary network outages, transient failures |
//!
//! ## Network Latency & Congestion
//!
//! | Delay Type | Config Field | Default | Real-World Scenario |
//! |------------|--------------|---------|---------------------|
//! | Operation latency | `bind/accept/connect/read/write_latency` | Various ranges | Timeout settings, async operation ordering |
//! | Latency distribution | `latency_distribution` | Uniform | Tail latency testing, P99 behavior |
//! | Slow latency | `slow_latency_probability` | 0.1% | 99.9th percentile testing |
//! | Write clogging | `clog_probability` + `clog_duration` | 0%, 100-300ms | Backpressure handling, flow control |
//! | Read clogging | Same as write | Same | Symmetric flow control |
//! | Clock drift | `clock_drift_enabled` + `clock_drift_max` | true, 100ms | Lease expiration, distributed consensus, TTL handling |
//! | Buggified delay | `buggified_delay_enabled` + `buggified_delay_max` | true, 100ms | Race conditions, timing-dependent bugs |
//! | Handshake delay | `handshake_delay_enabled` + `handshake_delay_max` | true, 10ms | TLS negotiation, connection startup overhead |
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
//! ## Half-Open Connection Simulation
//!
//! | State | Method | Real-World Scenario |
//! |-------|--------|---------------------|
//! | Peer crash | `simulate_peer_crash()` | TCP keepalive, heartbeat detection, silent failures |
//! | Half-open detection | `should_half_open_error()` | Timeout-based failure detection |
//!
//! ## Partial Write Simulation
//!
//! | Feature | Config Field | Default | Real-World Scenario |
//! |---------|--------------|---------|---------------------|
//! | Short writes | `partial_write_max_bytes` | 1000 bytes | TCP fragmentation handling, message framing |
//!
//! ## Stable Connections
//!
//! | Feature | Method | Real-World Scenario |
//! |---------|--------|---------------------|
//! | Mark stable | `mark_connection_stable()` | Exempt supervision from chaos, parent-child connections |
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
//! use moonpool_sim::network::{NetworkConfiguration, ChaosConfiguration, LatencyDistribution, PartitionStrategy};
//! use std::time::Duration;
//!
//! let mut config = NetworkConfiguration::default();
//! config.chaos.latency_distribution = LatencyDistribution::Bimodal;
//! config.chaos.partition_strategy = PartitionStrategy::IsolateSingle;
//! config.chaos.partition_probability = 0.05; // 5%
//! ```
//!
//! ## FDB/TigerBeetle References
//!
//! - Random close: FDB sim2.actor.cpp:580-605
//! - Latency distribution: FDB sim2.actor.cpp:317-329 (`halfLatency()`)
//! - Partitions: FDB SimClogging, TigerBeetle partition modes
//! - Bit flips: FDB FlowTransport.actor.cpp:1297
//! - Clock drift: FDB sim2.actor.cpp:1058-1064
//! - Connect failures: FDB sim2.actor.cpp:1243-1250

use crate::sim::rng::{sim_random_range, sim_random_range_or_default};
use std::ops::Range;
use std::time::Duration;

/// Latency distribution mode for network operations.
///
/// Controls how latencies are sampled for network operations.
/// FDB ref: sim2.actor.cpp:317-329 (`halfLatency()`)
///
/// # Real-World Scenario
///
/// Real networks exhibit bimodal latency patterns where most operations are fast,
/// but a small percentage experience significantly higher latency (tail latency).
/// This is crucial for testing:
/// - P99/P99.9 latency handling
/// - Timeout tuning
/// - Retry logic under tail latency conditions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LatencyDistribution {
    /// Uniform distribution within the configured range.
    /// All latencies equally likely within [min, max].
    #[default]
    Uniform,

    /// Bimodal distribution matching FDB's halfLatency() pattern.
    /// - 99.9% of operations: fast latency (within configured range)
    /// - 0.1% of operations: slow latency (multiplied by `slow_latency_multiplier`)
    ///
    /// FDB ref: sim2.actor.cpp:317-329
    Bimodal,
}

/// Network partition strategy for chaos testing.
///
/// Controls how nodes are selected for partitioning during chaos testing.
/// TigerBeetle ref: packet_simulator.zig:12-488
///
/// # Real-World Scenario
///
/// Different partition strategies test different failure modes:
/// - Random: General chaos, unpredictable failures
/// - UniformSize: Tests various quorum sizes and split scenarios
/// - IsolateSingle: Tests single-node isolation (common in production)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionStrategy {
    /// Random IP pairs selected for partitioning.
    /// Current behavior - randomly selects which connections to partition.
    #[default]
    Random,

    /// Uniform size partitions - randomly choose partition size from 1 to n-1 nodes.
    /// TigerBeetle pattern: creates partitions of varying sizes to test different
    /// quorum scenarios.
    UniformSize,

    /// Isolate single node - always partition exactly one node from the rest.
    /// Tests the common production scenario where a single node becomes unreachable.
    IsolateSingle,
}

/// Connection establishment failure mode for fault injection.
///
/// Controls how connection attempts fail during chaos testing.
/// FDB ref: sim2.actor.cpp:1243-1250 (SIM_CONNECT_ERROR_MODE)
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
/// following FoundationDB's BUGGIFY patterns for deterministic testing.
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
    /// When enabled, timer() can return a time up to clock_drift_max ahead of now()
    /// FDB ref: sim2.actor.cpp:1058-1064
    pub clock_drift_enabled: bool,

    /// Maximum clock drift (default 100ms per FDB)
    /// timer() can be up to this much ahead of now()
    pub clock_drift_max: Duration,

    /// Enable buggified delays on sleep/timer operations
    /// When enabled, 25% of sleep operations get extra delay
    /// FDB ref: sim2.actor.cpp:1100-1105
    pub buggified_delay_enabled: bool,

    /// Maximum additional delay for buggified sleep (default 100ms)
    /// Uses power-law distribution: max_delay * pow(random01(), 1000.0)
    /// FDB ref: sim2.actor.cpp:1104
    pub buggified_delay_max: Duration,

    /// Probability of adding buggified delay (default 25% per FDB)
    pub buggified_delay_probability: f64,

    /// Connection establishment failure mode (per FDB)
    /// FDB ref: sim2.actor.cpp:1243-1250 (SIM_CONNECT_ERROR_MODE)
    pub connect_failure_mode: ConnectFailureMode,

    /// Probability of connect failure when Probabilistic mode is enabled (default 50%)
    pub connect_failure_probability: f64,

    /// Latency distribution mode for network operations.
    ///
    /// Controls whether latencies follow a uniform or bimodal distribution.
    /// FDB ref: sim2.actor.cpp:317-329 (`halfLatency()`)
    ///
    /// # Real-World Scenario
    /// Bimodal latency tests tail latency handling (P99/P99.9).
    pub latency_distribution: LatencyDistribution,

    /// Probability of a slow latency sample when using bimodal distribution (0.0 - 1.0).
    /// FDB default: 0.001 (0.1%) - see sim2.actor.cpp:319
    ///
    /// # Real-World Scenario
    /// Tests handling of 99.9th percentile latencies.
    pub slow_latency_probability: f64,

    /// Multiplier for slow latencies in bimodal distribution.
    /// FDB: slow latency is up to 10x normal latency
    ///
    /// # Real-World Scenario
    /// Simulates network congestion, GC pauses, or cross-datacenter hops.
    pub slow_latency_multiplier: f64,

    /// Enable handshake delay simulation on new connections.
    /// FDB ref: connectHandshake():389 adds `delay(0.01 * random01())`
    ///
    /// # Real-World Scenario
    /// Simulates TLS negotiation, connection establishment overhead.
    pub handshake_delay_enabled: bool,

    /// Maximum handshake delay (default 10ms per FDB).
    /// Applied once when a connection is established.
    ///
    /// # Real-World Scenario
    /// Tests connection pool warm-up, startup latency.
    pub handshake_delay_max: Duration,

    /// Network partition strategy.
    /// Controls how nodes are selected for partitioning.
    /// TigerBeetle ref: packet_simulator.zig partition modes
    ///
    /// # Real-World Scenario
    /// Different strategies test different failure scenarios:
    /// - Random: unpredictable chaos
    /// - UniformSize: various quorum sizes
    /// - IsolateSingle: single node isolation (common in production)
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
            latency_distribution: LatencyDistribution::default(),
            slow_latency_probability: 0.001, // 0.1% per FDB halfLatency()
            slow_latency_multiplier: 10.0,   // 10x normal latency
            handshake_delay_enabled: true,
            handshake_delay_max: Duration::from_millis(10), // FDB: 0.01 * random01()
            partition_strategy: PartitionStrategy::default(),
        }
    }
}

impl ChaosConfiguration {
    /// Create a configuration with all chaos disabled (for fast local testing)
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
            latency_distribution: LatencyDistribution::Uniform, // No bimodal for fast testing
            slow_latency_probability: 0.0,                      // No slow latencies
            slow_latency_multiplier: 1.0,                       // No multiplier
            handshake_delay_enabled: false,                     // No handshake delays
            handshake_delay_max: Duration::ZERO,
            partition_strategy: PartitionStrategy::Random, // Default strategy
        }
    }

    /// Create a randomized chaos configuration for seed-based testing
    pub fn random_for_seed() -> Self {
        Self {
            clog_probability: sim_random_range(0..20) as f64 / 100.0, // 0-20% for clogging
            clog_duration: Duration::from_micros(sim_random_range(50000..300000))
                ..Duration::from_micros(sim_random_range(100000..500000)),
            partition_probability: sim_random_range(0..15) as f64 / 100.0, // 0-15% (lower than faults)
            partition_duration: Duration::from_millis(sim_random_range(100..1000))
                ..Duration::from_millis(sim_random_range(500..3000)),
            // Bit flip probability range: 0.001% to 0.02% (very low, like FDB)
            bit_flip_probability: sim_random_range(1..20) as f64 / 100000.0,
            bit_flip_min_bits: 1,
            bit_flip_max_bits: 32,
            bit_flip_cooldown: Duration::from_millis(sim_random_range(0..100)),
            partial_write_max_bytes: sim_random_range(100..2000), // Vary max bytes for different scenarios
            // Random close probability: 0.0001% to 0.01% (very low, like FDB)
            random_close_probability: sim_random_range(1..100) as f64 / 1000000.0,
            random_close_cooldown: Duration::from_millis(sim_random_range(1000..10000)),
            random_close_explicit_ratio: sim_random_range(20..40) as f64 / 100.0, // 20-40%
            clock_drift_enabled: true,
            clock_drift_max: Duration::from_millis(sim_random_range(50..150)), // 50-150ms
            buggified_delay_enabled: true,
            buggified_delay_max: Duration::from_millis(sim_random_range(50..150)), // 50-150ms
            buggified_delay_probability: sim_random_range(20..30) as f64 / 100.0,  // 20-30%
            connect_failure_mode: ConnectFailureMode::random_for_seed(),
            connect_failure_probability: sim_random_range(40..60) as f64 / 100.0, // 40-60%
            // Randomly choose latency distribution (50% uniform, 50% bimodal)
            latency_distribution: if sim_random_range(0..2) == 0 {
                LatencyDistribution::Uniform
            } else {
                LatencyDistribution::Bimodal
            },
            slow_latency_probability: sim_random_range(1..5) as f64 / 1000.0, // 0.1% to 0.5%
            slow_latency_multiplier: sim_random_range(5..20) as f64,          // 5x to 20x
            handshake_delay_enabled: true,
            handshake_delay_max: Duration::from_millis(sim_random_range(5..20)), // 5-20ms
            // Randomly choose partition strategy
            partition_strategy: match sim_random_range(0..3) {
                0 => PartitionStrategy::Random,
                1 => PartitionStrategy::UniformSize,
                _ => PartitionStrategy::IsolateSingle,
            },
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
pub fn sample_duration(range: &Range<Duration>) -> Duration {
    let start_nanos = range.start.as_nanos() as u64;
    let end_nanos = range.end.as_nanos() as u64;
    let random_nanos = sim_random_range_or_default(start_nanos..end_nanos);
    Duration::from_nanos(random_nanos)
}

/// Sample a random duration with bimodal distribution support.
///
/// FDB ref: sim2.actor.cpp:317-329 (`halfLatency()`)
///
/// When `distribution` is `Bimodal`:
/// - 99.9% of samples use normal latency (within `range`)
/// - 0.1% of samples use slow latency (multiplied by `slow_multiplier`)
///
/// # Parameters
/// - `range`: The base latency range
/// - `chaos`: Chaos configuration with distribution settings
///
/// # Real-World Scenario
/// Bimodal latency tests tail latency handling:
/// - Most requests complete quickly
/// - Rare requests experience significant delays (P99.9)
pub fn sample_duration_bimodal(range: &Range<Duration>, chaos: &ChaosConfiguration) -> Duration {
    use crate::sim::rng::sim_random;

    let base_duration = sample_duration(range);

    match chaos.latency_distribution {
        LatencyDistribution::Uniform => base_duration,
        LatencyDistribution::Bimodal => {
            // FDB pattern: 99.9% fast, 0.1% slow
            if sim_random::<f64>() < chaos.slow_latency_probability {
                // Slow path: multiply by slow_latency_multiplier
                let slow_nanos =
                    (base_duration.as_nanos() as f64 * chaos.slow_latency_multiplier) as u64;
                tracing::trace!(
                    "Bimodal slow latency: {:?} -> {:?}",
                    base_duration,
                    Duration::from_nanos(slow_nanos)
                );
                Duration::from_nanos(slow_nanos)
            } else {
                base_duration
            }
        }
    }
}

/// Sample a handshake delay based on chaos configuration.
///
/// FDB ref: connectHandshake():389 adds `delay(0.01 * random01())`
///
/// Returns `Duration::ZERO` if handshake delays are disabled.
pub fn sample_handshake_delay(chaos: &ChaosConfiguration) -> Duration {
    use crate::sim::rng::sim_random;

    if !chaos.handshake_delay_enabled || chaos.handshake_delay_max == Duration::ZERO {
        return Duration::ZERO;
    }

    // FDB pattern: 0.01 * random01() = up to 10ms
    let random_factor = sim_random::<f64>();
    Duration::from_nanos((chaos.handshake_delay_max.as_nanos() as f64 * random_factor) as u64)
}

impl NetworkConfiguration {
    /// Create a new network configuration with default settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a randomized network configuration for chaos testing
    pub fn random_for_seed() -> Self {
        Self {
            bind_latency: Duration::from_micros(sim_random_range(10..200))
                ..Duration::from_micros(sim_random_range(50..300)),
            accept_latency: Duration::from_micros(sim_random_range(1000..10000))
                ..Duration::from_micros(sim_random_range(5000..15000)),
            connect_latency: Duration::from_micros(sim_random_range(1000..50000))
                ..Duration::from_micros(sim_random_range(10000..100000)),
            read_latency: Duration::from_micros(sim_random_range(5..100))
                ..Duration::from_micros(sim_random_range(50..200)),
            write_latency: Duration::from_micros(sim_random_range(50..1000))
                ..Duration::from_micros(sim_random_range(200..2000)),
            chaos: ChaosConfiguration::random_for_seed(),
        }
    }

    /// Create a configuration optimized for fast local testing
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
