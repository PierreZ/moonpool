//! # moonpool-prometheus
//!
//! Turn the `prometheus` metrics your code already keeps into moonpool
//! simulation output.
//!
//! Register a [`PrometheusSource`] per simulated node. Every counter, gauge
//! and histogram it holds then appears in the simulation report, per node
//! and aggregated across seeds. Your application records its metrics the
//! same way as in production.
//!
//! The crate depends only on `moonpool-core` (the
//! [`MetricsSource`] vocabulary and
//! [`TimeProvider`](moonpool_core::TimeProvider)), never on `moonpool-sim`.
//! The same source works in production: hand
//! [`PrometheusSource::registry`] to your `/metrics` handler.
//!
//! ## Quick example
//!
//! A workload records a counter, a gauge and a latency histogram. The
//! values reach the report.
//!
//! ```rust
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! use async_trait::async_trait;
//! use moonpool_prometheus::PrometheusSource;
//! use moonpool_sim::{SimContext, SimulationBuilder, SimulationResult, TimeProvider, Workload};
//!
//! struct Metered;
//!
//! #[async_trait]
//! impl Workload for Metered {
//!     fn name(&self) -> &'static str {
//!         "metered"
//!     }
//!
//!     async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
//!         let metrics = ctx
//!             .metrics::<PrometheusSource>()
//!             .expect("metrics factory registered");
//!         let processed = metrics
//!             .counter("requests_total", "Requests processed")
//!             .expect("a valid metric name");
//!         let latency = metrics
//!             .histogram_with_buckets("request_latency_seconds", "Request latency", vec![0.005, 0.01, 0.05])
//!             .expect("ascending buckets");
//!         for _ in 0..3 {
//!             // Timed on the simulated clock, so a replay gives the same value.
//!             let timer = latency.start_timer(ctx.time());
//!             ctx.time().sleep(Duration::from_millis(10)).await.ok();
//!             timer.stop_and_record();
//!             processed.inc();
//!         }
//!         Ok(())
//!     }
//! }
//!
//! let report = SimulationBuilder::new()
//!     .set_debug_seeds(vec![42])
//!     .set_iterations(1)
//!     .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
//!     .workload(Metered)
//!     .run()
//!     .expect("simulation configuration is valid");
//!
//! let requests = report
//!     .app_metrics
//!     .iter()
//!     .find(|metric| metric.name == "requests_total")
//!     .expect("the counter is in the report");
//! assert!((requests.total - 3.0).abs() < f64::EPSILON);
//! ```
//!
//! ## Features
//!
//! - **Instrumented handles.** [`PrometheusSource::counter`],
//!   [`gauge`](PrometheusSource::gauge), [`histogram`](PrometheusSource::histogram),
//!   [`histogram_with_buckets`](PrometheusSource::histogram_with_buckets),
//!   [`histogram_with_opts`](PrometheusSource::histogram_with_opts) and the
//!   labelled families ([`counter_vec`](PrometheusSource::counter_vec),
//!   [`gauge_vec`](PrometheusSource::gauge_vec),
//!   [`histogram_vec`](PrometheusSource::histogram_vec)) return
//!   [`SimCounter`], [`SimGauge`], [`SimHistogram`] and their `Vec`
//!   variants. Each one wraps the real `prometheus` metric
//!   (`inner()`).
//! - **Simulated-time series.** Every mutation through a handle is one
//!   point in a time series, stamped with the simulated clock.
//! - **Final scrape.** Everything else in the registry is read once at the
//!   end of the iteration.
//! - **Deterministic timers.** [`SimHistogram::start_timer`] returns a
//!   [`SimTimer`] that reads the provider clock.
//! - **Bounded memory.** [`PrometheusSource::set_series_capacity`] caps the
//!   points kept per series.
//!
//! ## Two ways metrics reach the report
//!
//! **Instrumented handles push.** The handles above record every mutation
//! into a time series, stamped with the **simulated** clock. There is no
//! polling interval and no sampling: one point per `inc()`, `set()` and
//! `observe()`, at the simulated instant it happened. The series lands in
//! `SimulationMetrics::app_series`, ready to plot or assert on.
//!
//! **A final scrape catches the rest.** Other metrics in the registry
//! (metrics a library registered itself, `lazy_static!` globals, a
//! collector from another crate) have no mutation hook. The source reads
//! them once at the end of the iteration into
//! `SimulationMetrics::app_metrics`. You get their totals, not their
//! history.
//!
//! ## Feature flags
//!
//! None. The crate uses `prometheus` with `default-features = false`, which
//! drops the protobuf text and encoding stack. The facade crate `moonpool`
//! exposes this crate as `moonpool::prometheus` behind its `prometheus`
//! feature.
//!
//! ## Determinism
//!
//! Use [`SimHistogram::start_timer`], not `prometheus`' own `start_timer()`.
//! The `prometheus` timer reads the wall clock: it records host noise
//! instead of simulated latency, and it gives a different answer on every
//! replay of the same seed. This one reads the provider clock and replays
//! exactly.
//!
//! Metric values are reported, never used to steer the simulation.
//!
//! ## Reboots
//!
//! A source is keyed by node IP, so it outlives process reboots, exactly as
//! a real node's `/metrics` does across a restart on the same host. The
//! `counter`, `gauge` and `histogram` methods are therefore
//! get-or-register: a process that boots a second time looks its metrics up
//! instead of failing with `AlreadyReg`. The lookup checks the label names
//! and the histogram buckets: a second registration of a name with another
//! shape is an error, not the first metric.
//!
//! ## Examples
//!
//! `moonpool-sim-examples/src/metrics_service.rs` instruments a replicated
//! service and runs it under chaos: `cargo xtask sim run metrics-service`.
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

mod handles;

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moonpool_core::metrics::{
    HistogramValue, MetricClock, MetricPoint, MetricSample, MetricValue, MetricsSource,
    SeriesRecorder,
};
use prometheus::core::Collector;
use prometheus::{
    Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts,
    Registry,
};

pub use handles::{
    SimCounter, SimCounterVec, SimGauge, SimGaugeVec, SimHistogram, SimHistogramVec, SimTimer,
};

/// A `prometheus::Registry` the simulation can report on.
///
/// One per simulated node — see
/// `SimulationBuilder::metrics_factory`. Cheap to share behind an `Arc`, which
/// is how the simulation hands it to your code via `ctx.metrics()`.
pub struct PrometheusSource {
    registry: Registry,
    recorder: SeriesRecorder,
    /// Every metric registered through this source, by name, kept so a
    /// rebooted process can look it up, with the shape it was registered
    /// with.
    cache: Mutex<BTreeMap<String, Cached>>,
}

/// One metric in the cache of a [`PrometheusSource`].
struct Cached {
    metric: Box<dyn Any + Send>,
    shape: Shape,
}

/// What a second registration of a name must repeat: the label names, in
/// order, and the histogram buckets. The help text is not part of it.
#[derive(Debug, Clone, PartialEq, Default)]
struct Shape {
    labels: Vec<String>,
    buckets: Vec<f64>,
}

impl Shape {
    /// The shape of a family with these label names.
    fn labelled(labels: &[&str]) -> Self {
        Self {
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            buckets: Vec::new(),
        }
    }

    /// The shape of a histogram (family). Empty buckets are the default
    /// buckets, as in `prometheus`.
    fn histogram(opts: &HistogramOpts, labels: &[&str]) -> Self {
        let buckets = if opts.buckets.is_empty() {
            prometheus::DEFAULT_BUCKETS.to_vec()
        } else {
            opts.buckets.clone()
        };
        Self {
            buckets,
            ..Self::labelled(labels)
        }
    }
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
    /// already registered as a different metric kind or with a different
    /// shape (label names or buckets). A different help text is accepted:
    /// the first one stays.
    pub fn counter(&self, name: &str, help: &str) -> prometheus::Result<SimCounter> {
        let counter = self.get_or_register(name, "counter", Shape::default(), || {
            self.register(IntCounter::with_opts(Opts::new(name, help))?)
        })?;
        Ok(SimCounter::new(
            counter,
            name.to_owned(),
            self.recorder.clone(),
        ))
    }

    /// Get or register a gauge.
    ///
    /// # Errors
    ///
    /// As [`counter`](Self::counter).
    pub fn gauge(&self, name: &str, help: &str) -> prometheus::Result<SimGauge> {
        let gauge = self.get_or_register(name, "gauge", Shape::default(), || {
            self.register(Gauge::with_opts(Opts::new(name, help))?)
        })?;
        Ok(SimGauge::new(gauge, name.to_owned(), self.recorder.clone()))
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
        let shape = Shape::histogram(&opts, &[]);
        let histogram = self.get_or_register(&name, "histogram", shape, || {
            self.register(Histogram::with_opts(opts)?)
        })?;
        Ok(SimHistogram::new(histogram, name, self.recorder.clone()))
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
        let shape = Shape::labelled(labels);
        let family = self.get_or_register(name, "counter vec", shape.clone(), || {
            self.register(IntCounterVec::new(Opts::new(name, help), labels)?)
        })?;
        Ok(SimCounterVec::new(
            family,
            name.to_owned(),
            shape.labels,
            self.recorder.clone(),
        ))
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
        let shape = Shape::labelled(labels);
        let family = self.get_or_register(name, "gauge vec", shape.clone(), || {
            self.register(GaugeVec::new(Opts::new(name, help), labels)?)
        })?;
        Ok(SimGaugeVec::new(
            family,
            name.to_owned(),
            shape.labels,
            self.recorder.clone(),
        ))
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
        let shape = Shape::histogram(&opts, labels);
        let family = self.get_or_register(&name, "histogram vec", shape.clone(), || {
            self.register(HistogramVec::new(opts, labels)?)
        })?;
        Ok(SimHistogramVec::new(
            family,
            name,
            shape.labels,
            self.recorder.clone(),
        ))
    }

    /// Register `collector` with the wrapped registry and hand it back.
    fn register<C: Collector + Clone + 'static>(&self, collector: C) -> prometheus::Result<C> {
        self.registry.register(Box::new(collector.clone()))?;
        Ok(collector)
    }

    /// Look a metric up by name, registering it on first use.
    ///
    /// The cache is what makes reboots work: a process that boots again finds
    /// its metrics already registered rather than hitting `AlreadyReg`. A name
    /// cached as a different type than `M` is a kind clash, reported as
    /// wanting a `kind`. A name cached with a different [`Shape`] is a shape
    /// clash: to give back the cached metric would record the values of the
    /// caller under the wrong label names or in the wrong buckets. A
    /// different help text is accepted, and the first one stays.
    fn get_or_register<M: Clone + Send + 'static>(
        &self,
        name: &str,
        kind: &str,
        shape: Shape,
        create: impl FnOnce() -> prometheus::Result<M>,
    ) -> prometheus::Result<M> {
        let mut cache = self
            .cache
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        if let Some(existing) = cache.get(name) {
            let metric = existing
                .metric
                .downcast_ref::<M>()
                .cloned()
                .ok_or_else(|| kind_clash(name, kind))?;
            if existing.shape != shape {
                return Err(shape_clash(name, &existing.shape, &shape));
            }
            return Ok(metric);
        }
        let created = create()?;
        cache.insert(
            name.to_owned(),
            Cached {
                metric: Box::new(created.clone()),
                shape,
            },
        );
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

/// Error for a name already registered with other label names or buckets.
fn shape_clash(name: &str, registered: &Shape, wanted: &Shape) -> prometheus::Error {
    prometheus::Error::Msg(format!(
        "metric {name:?} is already registered with labels {:?} and buckets {:?} \
         (wanted labels {:?} and buckets {:?})",
        registered.labels, registered.buckets, wanted.labels, wanted.buckets
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_family_with_other_label_names_is_refused() {
        let source = PrometheusSource::default();
        source
            .counter_vec("requests_total", "Requests", &["method"])
            .expect("first registration");
        let Err(error) = source.counter_vec("requests_total", "Requests", &["code"]) else {
            panic!("other label names must be refused");
        };
        assert!(error.to_string().contains("already registered with labels"));
    }

    #[test]
    fn a_second_family_with_another_label_count_is_refused() {
        let source = PrometheusSource::default();
        source
            .gauge_vec("depth", "Depth", &["queue"])
            .expect("first registration");
        assert!(
            source
                .gauge_vec("depth", "Depth", &["queue", "node"])
                .is_err()
        );
        assert!(source.gauge_vec("depth", "Depth", &[]).is_err());
    }

    #[test]
    fn a_second_histogram_with_other_buckets_is_refused() {
        let source = PrometheusSource::default();
        source
            .histogram_with_buckets("latency", "Latency", vec![1.0, 2.0])
            .expect("first registration");
        assert!(
            source
                .histogram_with_buckets("latency", "Latency", vec![100.0, 200.0])
                .is_err()
        );
        let family = HistogramOpts::new("latency_by_node", "Latency").buckets(vec![1.0, 2.0]);
        source
            .histogram_vec(family, &["node"])
            .expect("first registration");
        let other = HistogramOpts::new("latency_by_node", "Latency").buckets(vec![5.0]);
        assert!(source.histogram_vec(other, &["node"]).is_err());
    }

    #[test]
    fn an_identical_second_registration_returns_the_cached_metric() {
        let source = PrometheusSource::default();
        let first = source
            .counter_vec("requests_total", "Requests", &["method"])
            .expect("first registration");
        // The help text is not part of the shape.
        let second = source
            .counter_vec("requests_total", "Other help", &["method"])
            .expect("same shape");
        first.with_label_values(&["GET"]).expect("one label").inc();
        assert_eq!(
            second.with_label_values(&["GET"]).expect("one label").get(),
            1
        );

        let histogram = source
            .histogram("latency", "Latency")
            .expect("first registration");
        histogram.observe(0.2);
        // The default buckets, given by value, are the same shape.
        let again = source
            .histogram_with_buckets("latency", "Latency", prometheus::DEFAULT_BUCKETS.to_vec())
            .expect("same buckets");
        assert_eq!(again.count(), 1);
    }
}
