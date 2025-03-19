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

    /// Generate a random duration within this range using the provided RNG
    pub fn sample<R: rand::Rng>(&self, rng: &mut R) -> Duration {
        if self.jitter.is_zero() {
            self.base
        } else {
            let jitter_nanos = rng.gen_range(0..=self.jitter.as_nanos() as u64);
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
