use crate::rng::sim_random_range;
use std::ops::Range;
use std::time::Duration;

/// Configuration for randomization ranges used in network parameter generation
#[derive(Debug, Clone)]
pub struct NetworkRandomizationRanges {
    /// Range for bind operation base latency (microseconds)
    pub bind_base_range: Range<u64>,
    /// Range for bind operation jitter latency (microseconds)
    pub bind_jitter_range: Range<u64>,
    /// Range for accept operation base latency (microseconds)
    pub accept_base_range: Range<u64>,
    /// Range for accept operation jitter latency (microseconds)
    pub accept_jitter_range: Range<u64>,
    /// Range for connect operation base latency (microseconds)
    pub connect_base_range: Range<u64>,
    /// Range for connect operation jitter latency (microseconds)
    pub connect_jitter_range: Range<u64>,
    /// Range for read operation base latency (microseconds)
    pub read_base_range: Range<u64>,
    /// Range for read operation jitter latency (microseconds)
    pub read_jitter_range: Range<u64>,
    /// Range for write operation base latency (microseconds)
    pub write_base_range: Range<u64>,
    /// Range for write operation jitter latency (microseconds)
    pub write_jitter_range: Range<u64>,
    /// Range for clogging probability (0.0 - 1.0)
    pub clogging_probability_range: Range<f64>,
    /// Range for clogging base duration (microseconds)
    pub clogging_base_duration_range: Range<u64>,
    /// Range for clogging jitter duration (microseconds)
    pub clogging_jitter_duration_range: Range<u64>,
}

impl Default for NetworkRandomizationRanges {
    fn default() -> Self {
        Self {
            bind_base_range: 10..200,                       // 10-200µs
            bind_jitter_range: 10..100,                     // 10-100µs
            accept_base_range: 1000..10000,                 // 1-10ms in µs
            accept_jitter_range: 1000..15000,               // 1-15ms in µs
            connect_base_range: 1000..50000,                // 1-50ms in µs
            connect_jitter_range: 5000..100000,             // 5-100ms in µs
            read_base_range: 5..100,                        // 5-100µs
            read_jitter_range: 10..200,                     // 10-200µs
            write_base_range: 50..1000,                     // 50-1000µs
            write_jitter_range: 100..2000,                  // 100-2000µs
            clogging_probability_range: 0.3..0.8,           // 30-80%
            clogging_base_duration_range: 100000..1000000,  // 100-1000ms in µs
            clogging_jitter_duration_range: 200000..800000, // 200-800ms in µs
        }
    }
}

/// Configuration for network simulation parameters
#[derive(Debug, Clone, Default)]
pub struct NetworkConfiguration {
    /// Latency configuration for various network operations
    pub latency: LatencyConfiguration,
    /// Network clogging configuration for write operations
    pub clogging: CloggingConfiguration,
}

/// Configuration for network operation latencies
#[derive(Debug, Clone)]
pub struct LatencyConfiguration {
    /// Base latency and jitter range for bind operations
    pub bind_latency: LatencyRange,
    /// Base latency and jitter range for accept operations
    pub accept_latency: LatencyRange,
    /// Base latency and jitter range for connect operations
    pub connect_latency: LatencyRange,
    /// Base latency and jitter range for read operations
    pub read_latency: LatencyRange,
    /// Base latency and jitter range for write operations
    pub write_latency: LatencyRange,
}

impl Default for LatencyConfiguration {
    fn default() -> Self {
        Self {
            bind_latency: LatencyRange::new(Duration::from_micros(50), Duration::from_micros(100)),
            accept_latency: LatencyRange::new(Duration::from_millis(1), Duration::from_millis(5)),
            connect_latency: LatencyRange::new(Duration::from_millis(1), Duration::from_millis(10)),
            read_latency: LatencyRange::new(Duration::from_micros(10), Duration::from_micros(50)),
            write_latency: LatencyRange::new(
                Duration::from_micros(100),
                Duration::from_micros(500),
            ),
        }
    }
}

impl LatencyConfiguration {
    /// Create randomized latency configuration with custom ranges
    pub fn random_with_ranges(ranges: &NetworkRandomizationRanges) -> Self {
        // Generate random latency parameters using the provided ranges (all in microseconds)
        let bind_base = sim_random_range(ranges.bind_base_range.clone());
        let bind_jitter = sim_random_range(ranges.bind_jitter_range.clone());

        let accept_base = sim_random_range(ranges.accept_base_range.clone());
        let accept_jitter = sim_random_range(ranges.accept_jitter_range.clone());

        let connect_base = sim_random_range(ranges.connect_base_range.clone());
        let connect_jitter = sim_random_range(ranges.connect_jitter_range.clone());

        let read_base = sim_random_range(ranges.read_base_range.clone());
        let read_jitter = sim_random_range(ranges.read_jitter_range.clone());

        let write_base = sim_random_range(ranges.write_base_range.clone());
        let write_jitter = sim_random_range(ranges.write_jitter_range.clone());

        Self {
            bind_latency: LatencyRange::new(
                Duration::from_micros(bind_base),
                Duration::from_micros(bind_jitter),
            ),
            accept_latency: LatencyRange::new(
                Duration::from_micros(accept_base),
                Duration::from_micros(accept_jitter),
            ),
            connect_latency: LatencyRange::new(
                Duration::from_micros(connect_base),
                Duration::from_micros(connect_jitter),
            ),
            read_latency: LatencyRange::new(
                Duration::from_micros(read_base),
                Duration::from_micros(read_jitter),
            ),
            write_latency: LatencyRange::new(
                Duration::from_micros(write_base),
                Duration::from_micros(write_jitter),
            ),
        }
    }

    /// Create randomized latency configuration based on the current simulation seed with default ranges
    pub fn random_for_seed() -> Self {
        Self::random_with_ranges(&NetworkRandomizationRanges::default())
    }
}

/// Range specification for latency with base duration and jitter
#[derive(Debug, Clone)]
pub struct LatencyRange {
    /// Base latency duration
    pub base: Duration,
    /// Maximum additional jitter duration (0 to this value)
    pub jitter: Duration,
}

impl LatencyRange {
    /// Create a new latency range
    pub fn new(base: Duration, jitter: Duration) -> Self {
        Self { base, jitter }
    }

    /// Create a fixed latency with no jitter
    pub fn fixed(duration: Duration) -> Self {
        Self {
            base: duration,
            jitter: Duration::ZERO,
        }
    }

    /// Generate a random duration within this range using thread-local RNG.
    ///
    /// This method uses the thread-local simulation RNG for deterministic
    /// randomness based on the current seed. The same seed will always
    /// produce the same sequence of latency values.
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::{LatencyRange, set_sim_seed};
    /// use std::time::Duration;
    ///
    /// set_sim_seed(42);
    /// let range = LatencyRange::new(Duration::from_millis(10), Duration::from_millis(5));
    /// let latency = range.sample();
    /// // latency will be between 10ms and 15ms, deterministic based on seed
    /// ```
    pub fn sample(&self) -> Duration {
        if self.jitter.is_zero() {
            self.base
        } else {
            let jitter_nanos = sim_random_range(0..(self.jitter.as_nanos() as u64 + 1));
            self.base + Duration::from_nanos(jitter_nanos)
        }
    }
}

/// Configuration for network clogging (temporary I/O blocking)
#[derive(Debug, Clone)]
pub struct CloggingConfiguration {
    /// Probability per write operation that clogging occurs (0.0 - 1.0)
    pub probability: f64,
    /// How long clogging lasts
    pub duration: LatencyRange,
}

impl Default for CloggingConfiguration {
    fn default() -> Self {
        Self {
            probability: 0.0, // No clogging by default
            duration: LatencyRange::new(Duration::from_millis(100), Duration::from_millis(200)),
        }
    }
}

impl CloggingConfiguration {
    /// Create randomized clogging configuration with custom ranges
    pub fn random_with_ranges(ranges: &NetworkRandomizationRanges) -> Self {
        // Generate random clogging parameters using the provided ranges (all in microseconds)
        let probability = sim_random_range(ranges.clogging_probability_range.clone());
        let base_duration = sim_random_range(ranges.clogging_base_duration_range.clone());
        let jitter_duration = sim_random_range(ranges.clogging_jitter_duration_range.clone());

        Self {
            probability,
            duration: LatencyRange::new(
                Duration::from_micros(base_duration),
                Duration::from_micros(jitter_duration),
            ),
        }
    }

    /// Create randomized clogging configuration based on the current simulation seed with default ranges
    pub fn random_for_seed() -> Self {
        Self::random_with_ranges(&NetworkRandomizationRanges::default())
    }
}

impl NetworkConfiguration {
    /// Create a new network configuration with custom latency settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a randomized network configuration with custom ranges
    pub fn random_with_ranges(ranges: &NetworkRandomizationRanges) -> Self {
        Self {
            latency: LatencyConfiguration::random_with_ranges(ranges),
            clogging: CloggingConfiguration::random_with_ranges(ranges),
        }
    }

    /// Create a randomized network configuration based on the current simulation seed with default ranges
    pub fn random_for_seed() -> Self {
        Self::random_with_ranges(&NetworkRandomizationRanges::default())
    }

    /// Create a configuration optimized for fast local testing (deterministic)
    pub fn fast_local() -> Self {
        Self {
            latency: LatencyConfiguration {
                bind_latency: LatencyRange::fixed(Duration::from_micros(1)),
                accept_latency: LatencyRange::fixed(Duration::from_micros(10)),
                connect_latency: LatencyRange::fixed(Duration::from_micros(10)),
                read_latency: LatencyRange::fixed(Duration::from_micros(1)),
                write_latency: LatencyRange::fixed(Duration::from_micros(1)),
            },
            clogging: CloggingConfiguration::default(), // No clogging for fast tests
        }
    }
}
