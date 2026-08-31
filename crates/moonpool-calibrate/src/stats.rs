//! Bounded latency accounting on top of an HDR histogram.
//!
//! Every measurement is recorded in nanoseconds into a
//! [`hdrhistogram::Histogram<u64>`], so a run keeps a constant amount of memory
//! no matter how many samples it takes. The reported envelope is `p01..p99`
//! rather than `min..max`: the extremes of a latency sample are dominated by
//! scheduler hiccups and page faults, which are not the steady-state behaviour a
//! simulation wants to reproduce.

use std::time::Duration;

use hdrhistogram::Histogram;

/// Lowest value the histogram can distinguish, in nanoseconds.
const LOWEST_NANOS: u64 = 1;

/// Highest value the histogram can record, in nanoseconds (60 seconds).
///
/// Comfortably above any single storage or small-message network operation; a
/// sample beyond it saturates rather than being dropped.
const HIGHEST_NANOS: u64 = 60_000_000_000;

/// Significant figures kept per value: 0.1% relative precision.
const SIGNIFICANT_FIGURES: u8 = 3;

/// Quantile used as the lower bound of the generated range.
const LOW_QUANTILE: f64 = 0.01;

/// Quantile used as the upper bound of the generated range.
const HIGH_QUANTILE: f64 = 0.99;

/// A running HDR histogram of operation latencies for one operation class.
#[derive(Debug)]
pub struct Latencies {
    /// Human-readable operation name (`read`, `write`, `sync`, `rtt`).
    name: &'static str,
    histogram: Histogram<u64>,
}

impl Latencies {
    /// Create an empty recorder for the named operation.
    ///
    /// # Panics
    ///
    /// Panics only if the compile-time bounds above become invalid, which would
    /// be a bug in this module rather than a runtime condition.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        let histogram =
            Histogram::new_with_bounds(LOWEST_NANOS, HIGHEST_NANOS, SIGNIFICANT_FIGURES)
                .expect("hdr histogram bounds are valid constants");
        Self { name, histogram }
    }

    /// Record one measured duration.
    ///
    /// Sub-nanosecond and above-range values saturate into the first and last
    /// bucket instead of being rejected, so a pathological outlier can never
    /// abort a calibration run.
    pub fn record(&mut self, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.histogram.saturating_record(nanos.max(LOWEST_NANOS));
    }

    /// Number of recorded samples.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.histogram.len()
    }

    /// Percentile summary of everything recorded so far.
    #[must_use]
    pub fn summary(&self) -> Summary {
        Summary {
            name: self.name,
            p01: self.quantile(LOW_QUANTILE),
            p50: self.quantile(0.50),
            p95: self.quantile(0.95),
            p99: self.quantile(HIGH_QUANTILE),
            max: Duration::from_nanos(self.histogram.max()),
            count: self.histogram.len(),
        }
    }

    fn quantile(&self, quantile: f64) -> Duration {
        Duration::from_nanos(self.histogram.value_at_quantile(quantile))
    }
}

/// The percentiles of one operation class, as reported on stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Operation name this summary describes.
    pub name: &'static str,
    /// 1st percentile latency — the lower bound of the generated range.
    pub p01: Duration,
    /// Median latency.
    pub p50: Duration,
    /// 95th percentile latency.
    pub p95: Duration,
    /// 99th percentile latency — the upper bound of the generated range.
    pub p99: Duration,
    /// Largest single sample seen.
    pub max: Duration,
    /// Number of recorded samples.
    pub count: u64,
}

impl Summary {
    /// The `p01..p99` envelope handed to the code generator.
    ///
    /// The upper bound is clamped to be at least the lower bound, so the
    /// generated `Uniform { start, end }` is never inverted even when every
    /// sample lands in a single histogram bucket.
    #[must_use]
    pub fn bounds(&self) -> Bounds {
        Bounds {
            start: self.p01,
            end: self.p99.max(self.p01),
        }
    }
}

/// A `start..end` latency envelope, mapping directly onto
/// `LatencyDistribution::Uniform { start, end }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Inclusive lower bound (measured p01).
    pub start: Duration,
    /// Exclusive upper bound (measured p99).
    pub end: Duration,
}

impl Bounds {
    /// Scale both bounds by a divisor, used to turn a round trip into the
    /// one-way delay moonpool's link latency knobs expect.
    #[must_use]
    pub fn divided_by(self, divisor: u32) -> Self {
        Self {
            start: self.start / divisor,
            end: self.end / divisor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bounds, Latencies};
    use std::time::Duration;

    #[test]
    fn quantiles_track_the_recorded_distribution() {
        let mut latencies = Latencies::new("read");
        for micros in 1..=1000 {
            latencies.record(Duration::from_micros(micros));
        }

        let summary = latencies.summary();
        assert_eq!(summary.count, 1000);
        // hdrhistogram reports the lower bound of the bucket a quantile falls
        // in, so allow the 0.1% precision the histogram is configured for.
        assert!(summary.p01 >= Duration::from_micros(9));
        assert!(summary.p01 <= Duration::from_micros(11));
        assert!(summary.p50 >= Duration::from_micros(499));
        assert!(summary.p50 <= Duration::from_micros(501));
        assert!(summary.p99 >= Duration::from_micros(989));
        assert!(summary.p99 <= Duration::from_micros(991));
        assert!(summary.max >= Duration::from_micros(999));
    }

    #[test]
    fn percentiles_are_ordered_and_bounds_are_never_inverted() {
        let mut latencies = Latencies::new("write");
        for micros in [7, 3, 900, 12, 45, 1, 300, 88, 2, 61] {
            latencies.record(Duration::from_micros(micros));
        }

        let summary = latencies.summary();
        assert!(summary.p01 <= summary.p50);
        assert!(summary.p50 <= summary.p95);
        assert!(summary.p95 <= summary.p99);
        assert!(summary.p99 <= summary.max);

        let bounds = summary.bounds();
        assert!(bounds.start <= bounds.end);
    }

    #[test]
    fn a_single_repeated_value_yields_a_degenerate_but_valid_range() {
        let mut latencies = Latencies::new("sync");
        for _ in 0..100 {
            latencies.record(Duration::from_micros(250));
        }

        let bounds = latencies.summary().bounds();
        assert!(bounds.start <= bounds.end);
        assert!(bounds.start > Duration::ZERO);
    }

    #[test]
    fn sub_nanosecond_samples_saturate_instead_of_being_dropped() {
        let mut latencies = Latencies::new("read");
        latencies.record(Duration::ZERO);
        assert_eq!(latencies.count(), 1);
        assert_eq!(latencies.summary().p50, Duration::from_nanos(1));
    }

    #[test]
    fn dividing_bounds_halves_a_round_trip() {
        let bounds = Bounds {
            start: Duration::from_micros(180),
            end: Duration::from_micros(740),
        };
        let one_way = bounds.divided_by(2);
        assert_eq!(one_way.start, Duration::from_micros(90));
        assert_eq!(one_way.end, Duration::from_micros(370));
        assert!(one_way.start <= one_way.end);
    }
}
