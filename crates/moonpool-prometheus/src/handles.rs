//! Instrumented metric handles that record every mutation.
//!
//! Each handle wraps the plain `prometheus` metric and, on every mutation,
//! pushes a point into the [`SeriesRecorder`] it shares with its
//! [`PrometheusSource`](crate::PrometheusSource). That is what makes a
//! simulation's metrics a *time series* rather than a final total: no polling
//! interval, no sampling, just the exact value at the exact simulated instant
//! it changed.
//!
//! Outside a simulation no clock is installed, so recording short-circuits on
//! one lock-free read and the handles behave like the metrics they wrap.

use std::time::Duration;

use moonpool_core::TimeProvider;
use moonpool_core::metrics::SeriesRecorder;
use prometheus::{Gauge, GaugeVec, Histogram, HistogramVec, IntCounter, IntCounterVec};

/// Widen a counter to `f64` for the recorded series.
///
/// Split into 32-bit halves rather than cast: `f64` holds every integer below
/// `2^53` exactly, and going through two `f64::from(u32)` conversions gets
/// there without a lossy primitive cast. Above `2^53` the result rounds, which
/// no simulation counter reaches.
pub(crate) fn counter_as_f64(value: u64) -> f64 {
    let high = f64::from(u32::try_from(value >> 32).unwrap_or(u32::MAX));
    let low = f64::from(u32::try_from(value & 0xFFFF_FFFF).unwrap_or(u32::MAX));
    high * 4_294_967_296.0 + low
}

/// A monotonically increasing counter that records each increment.
#[derive(Clone)]
pub struct SimCounter {
    inner: IntCounter,
    key: String,
    recorder: SeriesRecorder,
}

impl SimCounter {
    pub(crate) fn new(inner: IntCounter, key: String, recorder: SeriesRecorder) -> Self {
        Self {
            inner,
            key,
            recorder,
        }
    }

    /// Increment by one.
    pub fn inc(&self) {
        self.inner.inc();
        self.record();
    }

    /// Increment by `value`.
    pub fn inc_by(&self, value: u64) {
        self.inner.inc_by(value);
        self.record();
    }

    /// Current value.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.inner.get()
    }

    /// The wrapped `prometheus` counter, for code that needs the real type.
    ///
    /// Mutating through it bypasses series recording; the value still shows up
    /// in the end-of-iteration scrape.
    #[must_use]
    pub fn inner(&self) -> &IntCounter {
        &self.inner
    }

    fn record(&self) {
        self.recorder
            .record(&self.key, counter_as_f64(self.inner.get()));
    }
}

impl std::fmt::Debug for SimCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimCounter")
            .field("key", &self.key)
            .field("value", &self.inner.get())
            .finish_non_exhaustive()
    }
}

/// A gauge that records each new value.
#[derive(Clone)]
pub struct SimGauge {
    inner: Gauge,
    key: String,
    recorder: SeriesRecorder,
}

impl SimGauge {
    pub(crate) fn new(inner: Gauge, key: String, recorder: SeriesRecorder) -> Self {
        Self {
            inner,
            key,
            recorder,
        }
    }

    /// Set the gauge to `value`.
    pub fn set(&self, value: f64) {
        self.inner.set(value);
        self.record();
    }

    /// Increment by one.
    pub fn inc(&self) {
        self.inner.inc();
        self.record();
    }

    /// Decrement by one.
    pub fn dec(&self) {
        self.inner.dec();
        self.record();
    }

    /// Add `value` (negative subtracts).
    pub fn add(&self, value: f64) {
        self.inner.add(value);
        self.record();
    }

    /// Subtract `value`.
    pub fn sub(&self, value: f64) {
        self.inner.sub(value);
        self.record();
    }

    /// Current value.
    #[must_use]
    pub fn get(&self) -> f64 {
        self.inner.get()
    }

    /// The wrapped `prometheus` gauge. See [`SimCounter::inner`].
    #[must_use]
    pub fn inner(&self) -> &Gauge {
        &self.inner
    }

    fn record(&self) {
        self.recorder.record(&self.key, self.inner.get());
    }
}

impl std::fmt::Debug for SimGauge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimGauge")
            .field("key", &self.key)
            .field("value", &self.inner.get())
            .finish_non_exhaustive()
    }
}

/// A histogram that records each observation.
///
/// The scrape reports the bucket distribution; the recorded series reports the
/// individual observations in simulated-time order, which is what a latency
/// plot needs. Custom buckets come from
/// [`PrometheusSource::histogram_with_buckets`](crate::PrometheusSource::histogram_with_buckets).
#[derive(Clone)]
pub struct SimHistogram {
    inner: Histogram,
    key: String,
    recorder: SeriesRecorder,
}

impl SimHistogram {
    pub(crate) fn new(inner: Histogram, key: String, recorder: SeriesRecorder) -> Self {
        Self {
            inner,
            key,
            recorder,
        }
    }

    /// Observe `value`.
    pub fn observe(&self, value: f64) {
        self.inner.observe(value);
        // The observation itself, not a running total: a histogram's series is
        // the distribution over time, and summing it would hide the shape.
        self.recorder.record(&self.key, value);
    }

    /// Observe a duration, in seconds — Prometheus' convention for latency.
    pub fn observe_duration(&self, duration: Duration) {
        self.observe(duration.as_secs_f64());
    }

    /// Start a timer driven by the **simulated** clock.
    ///
    /// `prometheus`' own `start_timer()` reads the wall clock, which in a
    /// simulation records host noise instead of simulated latency and differs
    /// between replays of the same seed. This one reads `ctx.time()`, so the
    /// observation is exactly the latency the simulation modeled and replays
    /// identically.
    ///
    /// The timer observes on drop, so an early `?` return still records:
    ///
    /// ```ignore
    /// let _timer = latency.start_timer(ctx.time());
    /// let response = client.request(req).await?;
    /// ```
    #[must_use]
    pub fn start_timer<T: TimeProvider>(&self, time: &T) -> SimTimer<T> {
        SimTimer {
            histogram: self.clone(),
            start: time.now(),
            time: time.clone(),
            recorded: false,
        }
    }

    /// Number of observations so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.inner.get_sample_count()
    }

    /// Sum of all observations.
    #[must_use]
    pub fn sum(&self) -> f64 {
        self.inner.get_sample_sum()
    }

    /// The wrapped `prometheus` histogram. See [`SimCounter::inner`].
    #[must_use]
    pub fn inner(&self) -> &Histogram {
        &self.inner
    }
}

impl std::fmt::Debug for SimHistogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimHistogram")
            .field("key", &self.key)
            .field("count", &self.count())
            .field("sum", &self.sum())
            .finish_non_exhaustive()
    }
}

/// A running timer that observes simulated elapsed time into a histogram.
///
/// Created by [`SimHistogram::start_timer`]. Observes on drop unless stopped
/// explicitly, mirroring `prometheus::HistogramTimer`.
pub struct SimTimer<T: TimeProvider> {
    histogram: SimHistogram,
    time: T,
    start: Duration,
    recorded: bool,
}

impl<T: TimeProvider> SimTimer<T> {
    /// Observe the elapsed simulated time and return it, in seconds.
    pub fn stop_and_record(mut self) -> f64 {
        self.record()
    }

    /// Drop the timer without observing.
    pub fn stop_and_discard(mut self) {
        self.recorded = true;
    }

    /// Simulated time elapsed so far.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.time.now().saturating_sub(self.start)
    }

    fn record(&mut self) -> f64 {
        self.recorded = true;
        let elapsed = self.elapsed().as_secs_f64();
        self.histogram.observe(elapsed);
        elapsed
    }
}

impl<T: TimeProvider> Drop for SimTimer<T> {
    fn drop(&mut self) {
        if !self.recorded {
            self.record();
        }
    }
}

impl<T: TimeProvider> std::fmt::Debug for SimTimer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimTimer")
            .field("histogram", &self.histogram)
            .field("start", &self.start)
            .field("elapsed", &self.elapsed())
            .field("recorded", &self.recorded)
            .finish_non_exhaustive()
    }
}

/// A labelled counter family. Resolve a label combination with
/// [`with_label_values`](Self::with_label_values).
#[derive(Clone)]
pub struct SimCounterVec {
    inner: IntCounterVec,
    name: String,
    labels: Vec<String>,
    recorder: SeriesRecorder,
}

impl SimCounterVec {
    pub(crate) fn new(
        inner: IntCounterVec,
        name: String,
        labels: Vec<String>,
        recorder: SeriesRecorder,
    ) -> Self {
        Self {
            inner,
            name,
            labels,
            recorder,
        }
    }

    /// Get the counter for one label combination.
    ///
    /// # Errors
    ///
    /// Returns an error if `values` does not match the family's label count.
    pub fn with_label_values(&self, values: &[&str]) -> prometheus::Result<SimCounter> {
        let child = self.inner.get_metric_with_label_values(values)?;
        Ok(SimCounter::new(
            child,
            series_key(&self.name, &self.labels, values),
            self.recorder.clone(),
        ))
    }

    /// The wrapped `prometheus` family.
    #[must_use]
    pub fn inner(&self) -> &IntCounterVec {
        &self.inner
    }
}

/// A labelled gauge family.
#[derive(Clone)]
pub struct SimGaugeVec {
    inner: GaugeVec,
    name: String,
    labels: Vec<String>,
    recorder: SeriesRecorder,
}

impl SimGaugeVec {
    pub(crate) fn new(
        inner: GaugeVec,
        name: String,
        labels: Vec<String>,
        recorder: SeriesRecorder,
    ) -> Self {
        Self {
            inner,
            name,
            labels,
            recorder,
        }
    }

    /// Get the gauge for one label combination.
    ///
    /// # Errors
    ///
    /// Returns an error if `values` does not match the family's label count.
    pub fn with_label_values(&self, values: &[&str]) -> prometheus::Result<SimGauge> {
        let child = self.inner.get_metric_with_label_values(values)?;
        Ok(SimGauge::new(
            child,
            series_key(&self.name, &self.labels, values),
            self.recorder.clone(),
        ))
    }

    /// The wrapped `prometheus` family.
    #[must_use]
    pub fn inner(&self) -> &GaugeVec {
        &self.inner
    }
}

/// A labelled histogram family.
#[derive(Clone)]
pub struct SimHistogramVec {
    inner: HistogramVec,
    name: String,
    labels: Vec<String>,
    recorder: SeriesRecorder,
}

impl SimHistogramVec {
    pub(crate) fn new(
        inner: HistogramVec,
        name: String,
        labels: Vec<String>,
        recorder: SeriesRecorder,
    ) -> Self {
        Self {
            inner,
            name,
            labels,
            recorder,
        }
    }

    /// Get the histogram for one label combination.
    ///
    /// # Errors
    ///
    /// Returns an error if `values` does not match the family's label count.
    pub fn with_label_values(&self, values: &[&str]) -> prometheus::Result<SimHistogram> {
        let child = self.inner.get_metric_with_label_values(values)?;
        Ok(SimHistogram::new(
            child,
            series_key(&self.name, &self.labels, values),
            self.recorder.clone(),
        ))
    }

    /// The wrapped `prometheus` family.
    #[must_use]
    pub fn inner(&self) -> &HistogramVec {
        &self.inner
    }
}

/// Build the series identity for a labelled child: `name{key="value",...}`.
///
/// Label pairs are sorted so the key matches the one a scraped
/// `MetricSample` produces for the same series, letting the recorded series
/// and the final scrape line up.
fn series_key(name: &str, label_names: &[String], values: &[&str]) -> String {
    let mut pairs: Vec<String> = label_names
        .iter()
        .zip(values.iter())
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect();
    if pairs.is_empty() {
        return name.to_owned();
    }
    pairs.sort();
    format!("{name}{{{}}}", pairs.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_as_f64_is_exact_across_the_32_bit_boundary() {
        assert!((counter_as_f64(0) - 0.0).abs() < f64::EPSILON);
        assert!((counter_as_f64(1) - 1.0).abs() < f64::EPSILON);
        assert!((counter_as_f64(4_294_967_295) - 4_294_967_295.0).abs() < f64::EPSILON);
        assert!((counter_as_f64(4_294_967_296) - 4_294_967_296.0).abs() < f64::EPSILON);
        assert!((counter_as_f64(1_000_000_000_000) - 1_000_000_000_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn series_key_sorts_labels() {
        let names = vec!["method".to_owned(), "code".to_owned()];
        assert_eq!(
            series_key("requests_total", &names, &["GET", "200"]),
            r#"requests_total{code="200",method="GET"}"#
        );
        assert_eq!(series_key("uptime", &[], &[]), "uptime");
    }
}
