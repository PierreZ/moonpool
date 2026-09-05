//! Registry-agnostic application metrics.
//!
//! Production code is usually already instrumented with a metrics library —
//! a `prometheus::Registry`, an OpenTelemetry `MeterProvider`, and so on.
//! [`MetricsSource`] is the seam that lets the simulation *scrape* that
//! instrumentation without knowing which library produced it: an adapter
//! flattens whatever the library holds into [`MetricSample`]s, and
//! `moonpool-sim` folds those into its per-seed report.
//!
//! The vocabulary here is deliberately the smallest common denominator of
//! Prometheus and OpenTelemetry: a name, a set of labels, and a value that is
//! a counter, a gauge, or a histogram. Anything richer (exemplars, native
//! histograms, summaries with quantiles) degrades into that shape rather than
//! leaking a specific backend into the simulation runner.
//!
//! # Determinism
//!
//! Metric *values* are reported, never used to steer the simulation, because
//! not all of them are deterministic. In particular, timers that read the wall
//! clock (`prometheus::Histogram::start_timer`, `Instant::now()`) record host
//! noise and vary run to run. Record durations from the simulated clock
//! (`ctx.time()`) if you want a metric that is stable across replays of a seed.
//!
//! # Asking questions of the recorded series
//!
//! [`query`] turns the points a run recorded into numbers a report can print:
//! a small typed SELECT / RATE / BUCKETIZE / FILL / MAP / REDUCE model,
//! evaluated against one run's [`query::MetricSnapshot`].
//!
//! Adapters must emit samples in a deterministic order — sort by
//! [`MetricSample::sort_key`] if the backing registry does not already
//! guarantee one — so that two replays of the same seed produce byte-identical
//! reports.

pub mod query;
pub use query::f64_to_u64;

/// Widen a `u64` to `f64`, exactly for every value below `2^53`.
///
/// Split into 32-bit halves rather than cast: `f64` holds every integer below
/// `2^53` exactly, and going through two `f64::from(u32)` conversions gets
/// there without a lossy primitive cast. The alternative idiom,
/// `u32::try_from(v).map_or(f64::INFINITY, f64::from)`, silently turns any
/// value above `u32::MAX` into infinity — a rate across a gap longer than
/// about 49.7 simulated days would read as zero — which is why every widening
/// in the metrics engine goes through here.
///
/// At or above `2^53` the value is no longer representable: the high half is
/// scaled exactly (a multiply by a power of two) and the final addition rounds
/// once, so the result is the nearest `f64` to the integer, never infinity or
/// zero. No simulated clock or counter reaches that range.
#[must_use]
pub fn u64_to_f64_exact(value: u64) -> f64 {
    let high = f64::from(u32::try_from(value >> 32).unwrap_or(u32::MAX));
    let low = f64::from(u32::try_from(value & 0xFFFF_FFFF).unwrap_or(u32::MAX));
    high * 4_294_967_296.0 + low
}

/// The value carried by a single metric sample.
///
/// Counters and gauges are both `f64` (a Prometheus counter is a float even
/// when it is only ever incremented by one); the distinction is kept because
/// the two aggregate differently across simulation seeds — counters sum,
/// gauges average.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    /// A monotonically increasing total, e.g. `requests_total`.
    Counter(f64),
    /// A value that goes up and down, e.g. `queue_depth`.
    Gauge(f64),
    /// A bucketed distribution.
    Histogram(HistogramValue),
}

impl MetricValue {
    /// The scalar this sample contributes to a summary line: the counter or
    /// gauge value, or a histogram's sum.
    #[must_use]
    pub fn scalar(&self) -> f64 {
        match self {
            Self::Counter(v) | Self::Gauge(v) => *v,
            Self::Histogram(h) => h.sum,
        }
    }

    /// Short name of this value's kind, for display.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Counter(_) => "counter",
            Self::Gauge(_) => "gauge",
            Self::Histogram(_) => "histogram",
        }
    }
}

/// A bucketed distribution, in Prometheus' cumulative-bucket form.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HistogramValue {
    /// Total number of observations.
    pub count: u64,
    /// Sum of all observed values.
    pub sum: f64,
    /// `(upper_bound, cumulative_count)` pairs, ascending by bound.
    ///
    /// Cumulative, as Prometheus reports them: the count for a bucket includes
    /// every observation in the buckets below it. The implicit `+Inf` bucket
    /// (whose count equals [`count`](Self::count)) is not repeated here.
    pub buckets: Vec<(f64, u64)>,
}

impl HistogramValue {
    /// Mean of the observations, or `0.0` when nothing was observed.
    #[must_use]
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / u64_to_f64_exact(self.count)
        }
    }

    /// Merge another histogram over the same buckets into this one.
    ///
    /// Bucket bounds are expected to match; a bound present in `other` but not
    /// in `self` is appended, and the result is re-sorted by bound so the
    /// merged histogram stays ascending.
    pub fn merge(&mut self, other: &Self) {
        self.count += other.count;
        self.sum += other.sum;
        for (bound, count) in &other.buckets {
            if let Some(slot) = self
                .buckets
                .iter_mut()
                .find(|(b, _)| b.to_bits() == bound.to_bits())
            {
                slot.1 += count;
            } else {
                self.buckets.push((*bound, *count));
            }
        }
        self.buckets
            .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    }
}

/// One metric, at one label combination, at one point in time.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSample {
    /// Metric name, e.g. `http_requests_total`.
    pub name: String,
    /// Label pairs, sorted by key so that two samples of the same series
    /// always compare and format identically.
    pub labels: Vec<(String, String)>,
    /// The observed value.
    pub value: MetricValue,
}

impl MetricSample {
    /// Build a sample, sorting `labels` into canonical order.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        mut labels: Vec<(String, String)>,
        value: MetricValue,
    ) -> Self {
        labels.sort();
        Self {
            name: name.into(),
            labels,
            value,
        }
    }

    /// Add a label unless one with that key is already present.
    ///
    /// Used by the simulation runner to stamp each sample with the node it was
    /// scraped from, without clobbering a label the application already set.
    pub fn label_if_absent(&mut self, key: &str, value: &str) {
        if self.labels.iter().any(|(k, _)| k == key) {
            return;
        }
        self.labels.push((key.to_owned(), value.to_owned()));
        self.labels.sort();
    }

    /// Fully-qualified series identity: `name{key="value",...}`.
    ///
    /// Two samples with the same key are the same series and aggregate
    /// together across simulation seeds.
    #[must_use]
    pub fn sort_key(&self) -> String {
        if self.labels.is_empty() {
            return self.name.clone();
        }
        let mut key = String::with_capacity(self.name.len() + self.labels.len() * 16);
        key.push_str(&self.name);
        key.push('{');
        for (i, (k, v)) in self.labels.iter().enumerate() {
            if i > 0 {
                key.push(',');
            }
            key.push_str(k);
            key.push_str("=\"");
            key.push_str(v);
            key.push('"');
        }
        key.push('}');
        key
    }
}

/// One recorded value of a series, at one point in simulated time.
///
/// Points are pushed by instrumented metric handles as the application
/// mutates them — not sampled on a timer — so the series is exact: every
/// `inc()` and every `observe()` appears, at the simulated instant it
/// happened.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricPoint {
    /// Simulated time of the mutation, in milliseconds.
    pub time_ms: u64,
    /// The series' value after the mutation. For a histogram, the value that
    /// was observed rather than a running total.
    pub value: f64,
}

/// Supplies the timestamp stamped onto each [`MetricPoint`].
///
/// The simulation installs one backed by its logical clock, which is what
/// makes a recorded series deterministic: two replays of a seed produce
/// identical timestamps. Outside a simulation no clock is installed and
/// recording is inert.
pub trait MetricClock: Send + Sync + 'static {
    /// Current simulated time, in milliseconds.
    fn now_ms(&self) -> u64;
}

/// Default cap on points held per series before recording stops.
///
/// A hot loop can mutate a counter millions of times; keeping every point
/// would let a test's memory grow without bound. Once a series hits the cap it
/// stops growing and the overflow is counted, so a truncated series is visible
/// rather than silently misleading.
pub const DEFAULT_SERIES_CAPACITY: usize = 10_000;

/// Shared sink that instrumented metric handles push their mutations into.
///
/// Cheap to clone; every handle a source hands out shares one recorder, so a
/// single [`series`](Self::series) call returns the whole node's history.
/// Recording is inert until a [`MetricClock`] is installed — outside a
/// simulation the handles behave like their plain counterparts.
#[derive(Clone, Default)]
pub struct SeriesRecorder {
    inner: std::sync::Arc<SeriesRecorderInner>,
}

#[derive(Default)]
struct SeriesRecorderInner {
    clock: std::sync::RwLock<Option<std::sync::Arc<dyn MetricClock>>>,
    series: std::sync::Mutex<SeriesState>,
}

#[derive(Default)]
struct SeriesState {
    points: std::collections::BTreeMap<String, Vec<MetricPoint>>,
    capacity: Option<usize>,
    dropped: u64,
}

impl SeriesRecorder {
    /// Create an inert recorder — no clock, so nothing is recorded yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the clock that timestamps recorded points, arming recording.
    ///
    /// # Panics
    ///
    /// Panics if the recorder's lock is poisoned by a prior task panic.
    pub fn set_clock(&self, clock: std::sync::Arc<dyn MetricClock>) {
        *self
            .inner
            .clock
            .write()
            .expect("RwLock poisoned: prior task panicked") = Some(clock);
    }

    /// Set the per-series point cap. `None` restores
    /// [`DEFAULT_SERIES_CAPACITY`].
    ///
    /// # Panics
    ///
    /// Panics if the recorder's lock is poisoned by a prior task panic.
    pub fn set_capacity(&self, capacity: Option<usize>) {
        self.state().capacity = capacity;
    }

    /// Record `value` for the series named `key`, at the clock's current time.
    ///
    /// A no-op when no clock is installed, so instrumented handles cost one
    /// atomic read outside a simulation.
    ///
    /// # Panics
    ///
    /// Panics if the recorder's lock is poisoned by a prior task panic.
    pub fn record(&self, key: &str, value: f64) {
        let Some(time_ms) = self
            .inner
            .clock
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .as_ref()
            .map(|c| c.now_ms())
        else {
            return;
        };

        let mut state = self.state();
        let capacity = state.capacity.unwrap_or(DEFAULT_SERIES_CAPACITY);
        let entry = state.points.entry(key.to_owned()).or_default();
        if entry.len() >= capacity {
            state.dropped += 1;
            return;
        }
        entry.push(MetricPoint { time_ms, value });
    }

    /// Every recorded series, keyed by series identity, each ascending in time.
    ///
    /// # Panics
    ///
    /// Panics if the recorder's lock is poisoned by a prior task panic.
    #[must_use]
    pub fn series(&self) -> std::collections::BTreeMap<String, Vec<MetricPoint>> {
        self.state().points.clone()
    }

    /// Points dropped because their series was at capacity.
    ///
    /// # Panics
    ///
    /// Panics if the recorder's lock is poisoned by a prior task panic.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.state().dropped
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SeriesState> {
        self.inner
            .series
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }
}

impl std::fmt::Debug for SeriesRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        f.debug_struct("SeriesRecorder")
            .field("series", &state.points.len())
            .field("dropped", &state.dropped)
            .finish()
    }
}

/// A metrics backend the simulation can scrape.
///
/// Implemented by adapters over concrete registries — `moonpool-prometheus`
/// wraps a `prometheus::Registry` — and registered on the simulation builder
/// with `SimulationBuilder::metrics_factory`. The runner scrapes every source
/// once per iteration, after the check phase, and folds the samples into the
/// simulation report.
///
/// `Send + Sync` is a type-system constraint only: moonpool runs
/// single-threaded, but contexts must be `Send` to cross the executor.
pub trait MetricsSource: Send + Sync + 'static {
    /// Flatten the backing registry's current state into samples.
    ///
    /// Called once per simulation iteration. Implementations must produce a
    /// deterministic order (see the module docs) and must not mutate the
    /// registry — a scrape is an observation, and the same iteration may be
    /// replayed.
    fn collect(&self) -> Vec<MetricSample>;

    /// Arm event-driven recording against the simulated clock.
    ///
    /// Called once per node at the start of each iteration. A source that
    /// hands out instrumented handles installs the clock on its
    /// [`SeriesRecorder`]; one that only wraps a foreign registry leaves the
    /// default no-op and is captured by the end-of-iteration scrape instead.
    fn set_clock(&self, _clock: std::sync::Arc<dyn MetricClock>) {}

    /// The series recorded since the clock was installed, keyed by series
    /// identity (see [`MetricSample::sort_key`]).
    ///
    /// Default: no series, meaning this source is scrape-only.
    fn series(&self) -> std::collections::BTreeMap<String, Vec<MetricPoint>> {
        std::collections::BTreeMap::new()
    }

    /// Points this source dropped because a series hit its capacity.
    fn dropped_points(&self) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sorts_labels() {
        let s = MetricSample::new(
            "requests_total",
            vec![
                ("method".to_owned(), "GET".to_owned()),
                ("code".to_owned(), "200".to_owned()),
            ],
            MetricValue::Counter(3.0),
        );
        assert_eq!(s.labels[0].0, "code");
        assert_eq!(s.sort_key(), r#"requests_total{code="200",method="GET"}"#);
    }

    #[test]
    fn sort_key_without_labels_is_the_name() {
        let s = MetricSample::new("uptime", Vec::new(), MetricValue::Gauge(1.0));
        assert_eq!(s.sort_key(), "uptime");
    }

    #[test]
    fn label_if_absent_does_not_clobber() {
        let mut s = MetricSample::new(
            "requests_total",
            vec![("instance".to_owned(), "mine".to_owned())],
            MetricValue::Counter(1.0),
        );
        s.label_if_absent("instance", "10.0.1.1");
        assert_eq!(s.labels, vec![("instance".to_owned(), "mine".to_owned())]);

        s.label_if_absent("zone", "eu");
        assert_eq!(s.labels.len(), 2);
        assert_eq!(s.labels[0].0, "instance", "labels stay sorted");
    }

    #[test]
    fn histogram_merge_adds_counts_and_unions_bounds() {
        let mut a = HistogramValue {
            count: 2,
            sum: 3.0,
            buckets: vec![(1.0, 1), (5.0, 2)],
        };
        a.merge(&HistogramValue {
            count: 1,
            sum: 4.0,
            buckets: vec![(5.0, 1), (0.5, 0)],
        });
        assert_eq!(a.count, 3);
        assert!((a.sum - 7.0).abs() < f64::EPSILON);
        assert_eq!(a.buckets, vec![(0.5, 0), (1.0, 1), (5.0, 3)]);
    }

    #[test]
    fn histogram_mean_handles_empty() {
        assert!((HistogramValue::default().mean() - 0.0).abs() < f64::EPSILON);
        let h = HistogramValue {
            count: 4,
            sum: 10.0,
            buckets: Vec::new(),
        };
        assert!((h.mean() - 2.5).abs() < f64::EPSILON);
    }

    struct FixedClock(std::sync::atomic::AtomicU64);

    impl MetricClock for FixedClock {
        fn now_ms(&self) -> u64 {
            self.0.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[test]
    fn u64_to_f64_exact_is_exact_across_the_32_bit_boundary() {
        for value in [0, 1, 4_294_967_295, 4_294_967_296, 1_000_000_000_000] {
            let expected: f64 = value.to_string().parse().expect("decimal literal");
            assert!((u64_to_f64_exact(value) - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn u64_to_f64_exact_is_exact_up_to_two_pow_53() {
        let max_exact = (1_u64 << 53) - 1;
        assert!((u64_to_f64_exact(max_exact) - 9_007_199_254_740_991.0).abs() < f64::EPSILON);
        assert!((u64_to_f64_exact(1 << 53) - 9_007_199_254_740_992.0).abs() < f64::EPSILON);
        // Above 2^53 the result rounds to the nearest f64 rather than degrading
        // to zero or infinity.
        assert!((u64_to_f64_exact(u64::MAX) - 18_446_744_073_709_551_615.0).abs() < 1.0);
        assert!(u64_to_f64_exact(u64::MAX).is_finite());
    }

    #[test]
    fn histogram_mean_survives_counts_above_u32() {
        let histogram = HistogramValue {
            count: 5_000_000_000,
            sum: 10_000_000_000.0,
            buckets: Vec::new(),
        };
        assert!((histogram.mean() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn recorder_is_inert_without_a_clock() {
        let recorder = SeriesRecorder::new();
        recorder.record("hits", 1.0);
        assert!(recorder.series().is_empty(), "no clock, nothing recorded");
    }

    #[test]
    fn recorder_stamps_points_with_the_clock() {
        let clock = std::sync::Arc::new(FixedClock(std::sync::atomic::AtomicU64::new(10)));
        let recorder = SeriesRecorder::new();
        recorder.set_clock(clock.clone());

        recorder.record("hits", 1.0);
        clock.0.store(25, std::sync::atomic::Ordering::Relaxed);
        recorder.record("hits", 2.0);
        recorder.record("other", 7.0);

        let series = recorder.series();
        assert_eq!(series["hits"].len(), 2);
        assert_eq!(
            series["hits"][0],
            MetricPoint {
                time_ms: 10,
                value: 1.0
            }
        );
        assert_eq!(
            series["hits"][1],
            MetricPoint {
                time_ms: 25,
                value: 2.0
            }
        );
        assert_eq!(series["other"].len(), 1);
        assert_eq!(recorder.dropped(), 0);
    }

    #[test]
    fn recorder_caps_a_series_and_counts_the_overflow() {
        let recorder = SeriesRecorder::new();
        recorder.set_clock(std::sync::Arc::new(FixedClock(
            std::sync::atomic::AtomicU64::new(0),
        )));
        recorder.set_capacity(Some(2));

        for i in 0..5 {
            recorder.record("hits", f64::from(i));
        }

        assert_eq!(recorder.series()["hits"].len(), 2, "capped");
        assert_eq!(recorder.dropped(), 3, "overflow is visible, not silent");
    }

    #[test]
    fn scalar_reads_each_variant() {
        assert!((MetricValue::Counter(2.0).scalar() - 2.0).abs() < f64::EPSILON);
        assert!((MetricValue::Gauge(-1.0).scalar() + 1.0).abs() < f64::EPSILON);
        let h = MetricValue::Histogram(HistogramValue {
            count: 1,
            sum: 9.0,
            buckets: Vec::new(),
        });
        assert!((h.scalar() - 9.0).abs() < f64::EPSILON);
        assert_eq!(h.kind(), "histogram");
    }
}
