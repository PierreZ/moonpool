use crate::sim::rng::{sim_random_range, sim_random_range_or_default};
use std::ops::Range;
use std::time::Duration;

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
    /// 0 = disabled, 1 = always fail when buggified, 2 = probabilistic (50% fail, 50% hang)
    /// FDB ref: sim2.actor.cpp:1243-1250 (SIM_CONNECT_ERROR_MODE)
    pub connect_failure_mode: u8,

    /// Probability of connect failure when mode 2 is enabled (default 50%)
    pub connect_failure_probability: f64,
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
            connect_failure_mode: 2,           // FDB: SIM_CONNECT_ERROR_MODE = 2 (probabilistic)
            connect_failure_probability: 0.5,  // FDB: random01() > 0.5
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
            connect_failure_mode: 0, // Disabled
            connect_failure_probability: 0.5,
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
            // Randomly choose connect failure mode: 0, 1, or 2
            connect_failure_mode: sim_random_range(0..3) as u8,
            connect_failure_probability: sim_random_range(40..60) as f64 / 100.0, // 40-60%
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
