use crate::rng::sim_random_range;
use std::time::Duration;

/// Configuration for network simulation parameters
#[derive(Debug, Clone, Default)]
pub struct NetworkConfiguration {
    /// Latency configuration for various network operations
    pub latency: LatencyConfiguration,
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

impl NetworkConfiguration {
    /// Create a new network configuration with custom latency settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a configuration optimized for fast local testing
    pub fn fast_local() -> Self {
        Self {
            latency: LatencyConfiguration {
                bind_latency: LatencyRange::fixed(Duration::from_micros(1)),
                accept_latency: LatencyRange::fixed(Duration::from_micros(10)),
                connect_latency: LatencyRange::fixed(Duration::from_micros(10)),
                read_latency: LatencyRange::fixed(Duration::from_micros(1)),
                write_latency: LatencyRange::fixed(Duration::from_micros(1)),
            },
        }
    }

    /// Create a configuration simulating slower WAN conditions
    pub fn wan_simulation() -> Self {
        Self {
            latency: LatencyConfiguration {
                bind_latency: LatencyRange::new(Duration::from_millis(1), Duration::from_millis(2)),
                accept_latency: LatencyRange::new(
                    Duration::from_millis(10),
                    Duration::from_millis(20),
                ),
                connect_latency: LatencyRange::new(
                    Duration::from_millis(50),
                    Duration::from_millis(100),
                ),
                read_latency: LatencyRange::new(
                    Duration::from_millis(5),
                    Duration::from_millis(15),
                ),
                write_latency: LatencyRange::new(
                    Duration::from_millis(10),
                    Duration::from_millis(30),
                ),
            },
        }
    }
}
