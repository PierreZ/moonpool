use crate::rng::{sim_random_range, sim_random_range_or_default};
use std::ops::Range;
use std::time::Duration;

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

    /// Fault injection probability (0.0 - 1.0)
    pub fault_probability: f64,
    /// Duration range for fault delays
    pub fault_duration: Range<Duration>,
    /// Maximum faults per connection
    pub max_faults_per_connection: Option<u32>,
}

impl Default for NetworkConfiguration {
    fn default() -> Self {
        Self {
            bind_latency: Duration::from_micros(50)..Duration::from_micros(150),
            accept_latency: Duration::from_millis(1)..Duration::from_millis(6),
            connect_latency: Duration::from_millis(1)..Duration::from_millis(11),
            read_latency: Duration::from_micros(10)..Duration::from_micros(60),
            write_latency: Duration::from_micros(100)..Duration::from_micros(600),
            fault_probability: 0.0,
            fault_duration: Duration::from_millis(100)..Duration::from_millis(300),
            max_faults_per_connection: None,
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
            fault_probability: sim_random_range(0..30) as f64 / 100.0, // 0-30%
            fault_duration: Duration::from_micros(sim_random_range(50000..300000))
                ..Duration::from_micros(sim_random_range(100000..500000)),
            max_faults_per_connection: Some(sim_random_range(1..4)),
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
            fault_probability: 0.0,
            fault_duration: Duration::ZERO..Duration::ZERO,
            max_faults_per_connection: None,
        }
    }
}
