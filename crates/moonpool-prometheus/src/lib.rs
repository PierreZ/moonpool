//! # moonpool-prometheus
//!
//! Turn the `prometheus` metrics your code already keeps into moonpool
//! simulation output.
//!
//! Register a [`PrometheusSource`] per simulated node and every counter, gauge
//! and histogram it holds is reported at the end of the run — aggregated
//! across seeds, per node, with no changes to how your application records
//! them.
//!
//! ```ignore
//! use moonpool_prometheus::PrometheusSource;
//!
//! SimulationBuilder::new()
//!     .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
//!     .processes(3, || Box::new(MyNode::new()))
//!     .workload(MyWorkload::default())
//!     .run();
//! ```
//!
//! ## Two ways metrics reach the report
//!
//! **Instrumented handles push.** Metrics you create through this source —
//! [`counter`](PrometheusSource::counter), [`gauge`](PrometheusSource::gauge),
//! [`histogram`](PrometheusSource::histogram) — hand back wrappers that record
//! every mutation into a time series, stamped with the *simulated* clock. No
//! polling interval, no sampling: one point per `inc()`, `set()` and
//! `observe()`, at the simulated instant it happened. The series lands in
//! `SimulationMetrics::app_series`, ready to plot or assert on.
//!
//! **A final scrape catches the rest.** Anything else in the registry —
//! metrics a library registered itself, `lazy_static!` globals, a collector
//! from another crate — has no mutation hook to subscribe to, so it is read
//! once at the end of the iteration into `SimulationMetrics::app_metrics`. You
//! get totals for those, just not their history.
//!
//! ## Determinism
//!
//! Use [`SimHistogram::start_timer`] rather than `prometheus`' own
//! `start_timer()`: the latter reads the wall clock, so it records host noise
//! instead of simulated latency and gives a different answer on every replay
//! of the same seed. This one reads the provider clock and replays exactly.
//!
//! Metric values are reported, never used to steer the simulation.
//!
//! ## Reboots
//!
//! A source is keyed by node IP, so it outlives process reboots — exactly as a
//! real node's `/metrics` does across a restart on the same host. The
//! `counter` / `gauge` / `histogram` methods are therefore get-or-register: a
//! process that boots a second time looks its metrics up instead of failing
//! with `AlreadyReg`.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

mod handles;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moonpool_core::metrics::{
    HistogramValue, MetricClock, MetricPoint, MetricSample, MetricValue, MetricsSource,
    SeriesRecorder,
};
use prometheus::{
    Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts,
    Registry,
};

pub use handles::{
    SimCounter, SimCounterVec, SimGauge, SimGaugeVec, SimHistogram, SimHistogramVec, SimTimer,
};

/// A metric already registered, kept so a rebooted process can look it up.
#[derive(Clone)]
enum Cached {
    Counter(IntCounter),
    Gauge(Gauge),
    Histogram(Histogram),
    CounterVec(IntCounterVec, Vec<String>),
    GaugeVec(GaugeVec, Vec<String>),
    HistogramVec(HistogramVec, Vec<String>),
}

/// A `prometheus::Registry` the simulation can report on.
///
/// One per simulated node — see
/// `SimulationBuilder::metrics_factory`. Cheap to share behind an `Arc`, which
/// is how the simulation hands it to your code via `ctx.metrics()`.
pub struct PrometheusSource {
    registry: Registry,
    recorder: SeriesRecorder,
    cache: Mutex<BTreeMap<String, Cached>>,
}

impl Default for PrometheusSource {
    fn default() -> Self {
        Self::new(Registry::new())
    }
}

impl PrometheusSource {
    /// Wrap an existing registry.
    ///
    /// Use this when your application already builds its own `Registry` and
    /// passes it around. To scrape the crate-global default registry, pass
    /// `prometheus::default_registry().clone()` — but note it is process-wide,
    /// so its counters accumulate across seeds and nodes rather than starting
    /// fresh per iteration.
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            recorder: SeriesRecorder::new(),
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// The wrapped registry, for handing to code that expects the real type
    /// (an HTTP `/metrics` handler, a library that registers its own
    /// collectors).
    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The shared series recorder backing this source's instrumented handles.
    #[must_use]
    pub fn recorder(&self) -> &SeriesRecorder {
        &self.recorder
    }

    /// Cap the points held per recorded series. `None` restores the default.
    pub fn set_series_capacity(&self, capacity: Option<usize>) {
        self.recorder.set_capacity(capacity);
    }

    /// Get or register a counter.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a valid metric name, or if it is
    /// already registered as a different metric kind.
    pub fn counter(&self, name: &str, help: &str) -> prometheus::Result<SimCounter> {
        let metric = self.get_or_register(name, || {
            let counter = IntCounter::with_opts(Opts::new(name, help))?;
            self.registry.register(Box::new(counter.clone()))?;
            Ok(Cached::Counter(counter))
        })?;
        match metric {
            Cached::Counter(counter) => Ok(SimCounter::new(
                counter,
                name.to_owned(),
                self.recorder.clone(),
            )),
            _ => Err(kind_clash(name, "counter")),
        }
    }

    /// Get or register a gauge.
    ///
    /// # Errors
    ///
    /// As [`counter`](Self::counter).
    pub fn gauge(&self, name: &str, help: &str) -> prometheus::Result<SimGauge> {
        let metric = self.get_or_register(name, || {
            let gauge = Gauge::with_opts(Opts::new(name, help))?;
            self.registry.register(Box::new(gauge.clone()))?;
            Ok(Cached::Gauge(gauge))
        })?;
        match metric {
            Cached::Gauge(gauge) => {
                Ok(SimGauge::new(gauge, name.to_owned(), self.recorder.clone()))
            }
            _ => Err(kind_clash(name, "gauge")),
        }
    }

    /// Get or register a histogram with Prometheus' default buckets.
    ///
    /// # Errors
    ///
    /// As [`counter`](Self::counter).
    pub fn histogram(&self, name: &str, help: &str) -> prometheus::Result<SimHistogram> {
        self.histogram_with_opts(HistogramOpts::new(name, help))
    }

    /// Get or register a histogram with explicit bucket bounds.
    ///
    /// Default buckets are tuned for HTTP latency in seconds. Simulated
    /// latencies often live on a different scale, so pick bounds that bracket
    /// what the simulation actually produces — a distribution that lands
    /// entirely in the last bucket tells you nothing.
    ///
    /// # Errors
    ///
    /// As [`counter`](Self::counter), plus an error if `buckets` is not
    /// strictly ascending.
    pub fn histogram_with_buckets(
        &self,
        name: &str,
        help: &str,
        buckets: Vec<f64>,
    ) -> prometheus::Result<SimHistogram> {
        self.histogram_with_opts(HistogramOpts::new(name, help).buckets(buckets))
    }

    /// Get or register a histogram from full `prometheus` options.
    ///
    /// # Errors
    ///
    /// As [`histogram_with_buckets`](Self::histogram_with_buckets).
    pub fn histogram_with_opts(&self, opts: HistogramOpts) -> prometheus::Result<SimHistogram> {
        let name = opts.common_opts.name.clone();
        let metric = self.get_or_register(&name, || {
            let histogram = Histogram::with_opts(opts)?;
            self.registry.register(Box::new(histogram.clone()))?;
            Ok(Cached::Histogram(histogram))
        })?;
        match metric {
            Cached::Histogram(histogram) => Ok(SimHistogram::new(
                histogram,
                name.clone(),
                self.recorder.clone(),
            )),
            _ => Err(kind_clash(&name, "histogram")),
        }
    }

    /// Get or register a labelled counter family.
    ///
    /// # Errors
    ///
    /// As [`counter`](Self::counter).
    pub fn counter_vec(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
    ) -> prometheus::Result<SimCounterVec> {
        let owned: Vec<String> = labels.iter().map(|l| (*l).to_owned()).collect();
        let metric = self.get_or_register(name, || {
            let family = IntCounterVec::new(Opts::new(name, help), labels)?;
            self.registry.register(Box::new(family.clone()))?;
            Ok(Cached::CounterVec(family, owned))
        })?;
        match metric {
            Cached::CounterVec(family, labels) => Ok(SimCounterVec::new(
                family,
                name.to_owned(),
                labels,
                self.recorder.clone(),
            )),
            _ => Err(kind_clash(name, "counter vec")),
        }
    }

    /// Get or register a labelled gauge family.
    ///
    /// # Errors
    ///
    /// As [`counter`](Self::counter).
    pub fn gauge_vec(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
    ) -> prometheus::Result<SimGaugeVec> {
        let owned: Vec<String> = labels.iter().map(|l| (*l).to_owned()).collect();
        let metric = self.get_or_register(name, || {
            let family = GaugeVec::new(Opts::new(name, help), labels)?;
            self.registry.register(Box::new(family.clone()))?;
            Ok(Cached::GaugeVec(family, owned))
        })?;
        match metric {
            Cached::GaugeVec(family, labels) => Ok(SimGaugeVec::new(
                family,
                name.to_owned(),
                labels,
                self.recorder.clone(),
            )),
            _ => Err(kind_clash(name, "gauge vec")),
        }
    }

    /// Get or register a labelled histogram family.
    ///
    /// # Errors
    ///
    /// As [`histogram_with_buckets`](Self::histogram_with_buckets).
    pub fn histogram_vec(
        &self,
        opts: HistogramOpts,
        labels: &[&str],
    ) -> prometheus::Result<SimHistogramVec> {
        let name = opts.common_opts.name.clone();
        let owned: Vec<String> = labels.iter().map(|l| (*l).to_owned()).collect();
        let metric = self.get_or_register(&name, || {
            let family = HistogramVec::new(opts, labels)?;
            self.registry.register(Box::new(family.clone()))?;
            Ok(Cached::HistogramVec(family, owned))
        })?;
        match metric {
            Cached::HistogramVec(family, labels) => Ok(SimHistogramVec::new(
                family,
                name.clone(),
                labels,
                self.recorder.clone(),
            )),
            _ => Err(kind_clash(&name, "histogram vec")),
        }
    }

    /// Look a metric up by name, registering it on first use.
    ///
    /// The cache is what makes reboots work: a process that boots again finds
    /// its metrics already registered rather than hitting `AlreadyReg`.
    fn get_or_register(
        &self,
        name: &str,
        create: impl FnOnce() -> prometheus::Result<Cached>,
    ) -> prometheus::Result<Cached> {
        let mut cache = self
            .cache
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        if let Some(existing) = cache.get(name) {
            return Ok(existing.clone());
        }
        let created = create()?;
        cache.insert(name.to_owned(), created.clone());
        Ok(created)
    }
}

impl std::fmt::Debug for PrometheusSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusSource")
            .field(
                "metrics",
                &self.cache.lock().map(|c| c.len()).unwrap_or_default(),
            )
            .field("recorder", &self.recorder)
            .finish_non_exhaustive()
    }
}

impl MetricsSource for PrometheusSource {
    fn collect(&self) -> Vec<MetricSample> {
        let mut samples = Vec::new();
        // `gather()` returns families sorted by name, and we sort the final
        // set again downstream, so a scrape is order-stable across replays.
        for family in self.registry.gather() {
            let name = family.name().to_owned();
            for metric in family.get_metric() {
                let labels: Vec<(String, String)> = metric
                    .get_label()
                    .iter()
                    .map(|pair| (pair.name().to_owned(), pair.value().to_owned()))
                    .collect();
                let Some(value) = read_value(family.get_field_type(), metric) else {
                    continue;
                };
                samples.push(MetricSample::new(name.clone(), labels, value));
            }
        }
        samples
    }

    fn set_clock(&self, clock: Arc<dyn MetricClock>) {
        self.recorder.set_clock(clock);
    }

    fn series(&self) -> BTreeMap<String, Vec<MetricPoint>> {
        self.recorder.series()
    }

    fn dropped_points(&self) -> u64 {
        self.recorder.dropped()
    }
}

/// Map one `prometheus` metric onto the registry-agnostic value model.
///
/// A summary degrades to a histogram carrying only count and sum: its
/// quantiles are computed client-side and have no bucket representation.
fn read_value(
    kind: prometheus::proto::MetricType,
    metric: &prometheus::proto::Metric,
) -> Option<MetricValue> {
    match kind {
        prometheus::proto::MetricType::COUNTER => {
            Some(MetricValue::Counter(metric.get_counter().get_value()))
        }
        prometheus::proto::MetricType::GAUGE => {
            Some(MetricValue::Gauge(metric.get_gauge().get_value()))
        }
        // `Untyped` is deprecated in the prometheus crate with no replacement
        // and no public API that produces one, so there is nothing to read.
        prometheus::proto::MetricType::UNTYPED => None,
        prometheus::proto::MetricType::HISTOGRAM => {
            let histogram = metric.get_histogram();
            Some(MetricValue::Histogram(HistogramValue {
                count: histogram.get_sample_count(),
                sum: histogram.get_sample_sum(),
                buckets: histogram
                    .get_bucket()
                    .iter()
                    .map(|b| (b.upper_bound(), b.cumulative_count()))
                    .collect(),
            }))
        }
        prometheus::proto::MetricType::SUMMARY => {
            let summary = metric.get_summary();
            Some(MetricValue::Histogram(HistogramValue {
                count: summary.sample_count(),
                sum: summary.sample_sum(),
                buckets: Vec::new(),
            }))
        }
    }
}

/// Error for a name already registered under a different metric kind.
fn kind_clash(name: &str, wanted: &str) -> prometheus::Error {
    prometheus::Error::Msg(format!(
        "metric {name:?} is already registered as a different kind (wanted a {wanted})"
    ))
}
