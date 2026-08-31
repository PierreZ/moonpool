//! Typed metric queries over one run's recorded series.
//!
//! [`SeriesRecorder`](super::SeriesRecorder) gives every simulation run an
//! exact history of each metric: one [`MetricPoint`] per `inc()` / `set()` /
//! `observe()`, stamped with the simulated clock. That is enough to answer
//! real questions — "what throughput did this seed sustain?", "which seed had
//! the worst p99?" — but only if something turns points into numbers. This
//! module is that something.
//!
//! The vocabulary is Warp 10's, cut down to what a simulation report needs:
//!
//! | Op | Meaning |
//! |----|---------|
//! | `SELECT` | pick series by metric name and label matchers |
//! | `RATE` | monotonic counter → per-second rate, on simulated time |
//! | `BUCKETIZE` | regularize points into fixed simulated-time buckets |
//! | `FILL` | give the empty buckets a value, Warp 10 style |
//! | `MAP` | rolling window over one series' points |
//! | `REDUCE` | combine across series, optionally grouped by a label |
//!
//! There is no expression language, no parser and no storage engine: a query
//! is a Rust value built by a typed builder, and it runs against the
//! [`MetricSnapshot`] one simulation iteration produced.
//!
//! ```
//! use std::time::Duration;
//! use moonpool_core::metrics::query::{Fill, Mean, MetricQuery, Percentile};
//!
//! let throughput = MetricQuery::select("requests_total")
//!     .label("operation", "write")
//!     .rate()
//!     .bucketize(Duration::from_secs(60), Mean)
//!     // A minute with no requests is zero per second, not missing data.
//!     .fill(Fill::Value(0.0))
//!     .named("write_throughput");
//!
//! let tail = MetricQuery::select("request_latency_seconds")
//!     .bucketize(Duration::from_secs(60), Percentile(0.99))
//!     .named("write_p99");
//!
//! assert_eq!(throughput.name(), "write_throughput");
//! assert_eq!(tail.name(), "write_p99");
//! ```
//!
//! # Percentile correctness
//!
//! `mean(p99(node_a), p99(node_b))` is not a p99, and `p99(p99(a), p99(b))` is
//! not one either. A percentile is only meaningful while the individual
//! observations beneath it are still there, so the builder tracks what each
//! stage's values *are* in the type system:
//!
//! | Stage | Values are | `Percentile` allowed? |
//! |-------|-----------|-----------------------|
//! | [`Observations`] | individual observations (or rates between consecutive ones) | yes |
//! | [`Scalar`] | already collapsed by min/mean/max | no |
//! | [`Quantile`] | already collapsed by a percentile | no |
//!
//! [`Min`], [`Mean`] and [`Max`] apply at any stage — "the mean of each node's
//! p99" is a fine number as long as nobody calls it a p99, and the report
//! labels it by the query's [`Provenance`]. [`Percentile`] only implements
//! [`ValidOn<Observations>`], so the invalid compositions fail to compile:
//!
//! ```compile_fail
//! use std::time::Duration;
//! use moonpool_core::metrics::query::{MetricQuery, Percentile};
//!
//! // p99 of per-bucket p99s: rejected, the observations are already gone.
//! MetricQuery::select("request_latency_seconds")
//!     .bucketize(Duration::from_secs(60), Percentile(0.99))
//!     .reduce(Percentile(0.99))
//!     .named("nonsense");
//! ```
//!
//! ```compile_fail
//! use std::time::Duration;
//! use moonpool_core::metrics::query::{Mean, MetricQuery, Percentile};
//!
//! // p99 of per-bucket means: rejected for the same reason.
//! MetricQuery::select("request_latency_seconds")
//!     .bucketize(Duration::from_secs(60), Mean)
//!     .reduce(Percentile(0.99))
//!     .named("nonsense");
//! ```
//!
//! Aggregating *across runs* is a different dimension and stays legitimate:
//! each seed contributes one equally-weighted value, so "the p95 seed's p99
//! latency" is a real statistic. [`MetricQueryReport`] computes it and names
//! it as across-run.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use super::{MetricPoint, MetricSample};

// ---------------------------------------------------------------------------
// Series identity
// ---------------------------------------------------------------------------

/// A Prometheus series identity: a metric name plus a label set.
///
/// Labels are kept sorted by key, so two label sets that differ only in
/// insertion order are the same series. [`Display`](fmt::Display) renders the
/// canonical form `name{key="value",...}` — byte-identical to
/// [`MetricSample::sort_key`], which is what recorded series are keyed by.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesKey {
    /// Metric name, e.g. `http_requests_total`.
    pub metric: String,
    /// Label pairs, sorted by key.
    pub labels: Vec<(String, String)>,
}

impl SeriesKey {
    /// Build a key, sorting `labels` into canonical order.
    #[must_use]
    pub fn new(metric: impl Into<String>, mut labels: Vec<(String, String)>) -> Self {
        labels.sort();
        Self {
            metric: metric.into(),
            labels,
        }
    }

    /// Parse the canonical `name{key="value",...}` form.
    ///
    /// Anything that is not that shape is taken as a bare metric name with no
    /// labels. Label values are not unescaped: moonpool's own key formatter
    /// does not escape them either, so a value containing `,` or `"` round
    /// trips inexactly — keep such characters out of label values.
    #[must_use]
    pub fn parse(key: &str) -> Self {
        let Some((metric, rest)) = key.split_once('{') else {
            return Self::new(key, Vec::new());
        };
        let Some(inner) = rest.strip_suffix('}') else {
            return Self::new(key, Vec::new());
        };
        let labels = inner
            .split(',')
            .filter(|part| !part.is_empty())
            .filter_map(|part| {
                let (k, v) = part.split_once('=')?;
                let v = v.trim_matches('"');
                Some((k.to_owned(), v.to_owned()))
            })
            .collect();
        Self::new(metric, labels)
    }

    /// The value of `key`, when this series carries that label.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Whether this series has `metric` as its name and carries every matcher.
    ///
    /// Matching is by subset: a series may carry labels the query does not
    /// mention, but every label the query names must be present with exactly
    /// that value.
    #[must_use]
    pub fn matches(&self, metric: &str, matchers: &[(String, String)]) -> bool {
        self.metric == metric
            && matchers
                .iter()
                .all(|(k, v)| self.label(k).is_some_and(|found| found == v))
    }
}

impl fmt::Display for SeriesKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.metric)?;
        if self.labels.is_empty() {
            return Ok(());
        }
        f.write_str("{")?;
        for (i, (k, v)) in self.labels.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{k}=\"{v}\"")?;
        }
        f.write_str("}")
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Every series one simulation run produced, in query-ready form.
///
/// Built from a run's recorded series plus its end-of-run scrape. A series the
/// application recorded through instrumented handles keeps its full history; a
/// series that only appears in the scrape (a foreign registry, a `lazy_static!`
/// global) contributes a single point at the run's end time, which is enough
/// for [`rate`](MetricQuery::rate) to read as "this much, over the whole run".
#[derive(Debug, Clone, Default)]
pub struct MetricSnapshot {
    series: BTreeMap<SeriesKey, Vec<MetricPoint>>,
    end_time_ms: u64,
}

impl MetricSnapshot {
    /// An empty snapshot for a run that ended at `end_time_ms`.
    #[must_use]
    pub fn new(end_time_ms: u64) -> Self {
        Self {
            series: BTreeMap::new(),
            end_time_ms,
        }
    }

    /// Build a snapshot from one iteration's metrics.
    ///
    /// `series` is keyed by [`MetricSample::sort_key`] (what
    /// `SimulationMetrics::app_series` holds); `samples` is the end-of-run
    /// scrape. Recorded series win, and any scraped series without a recorded
    /// history is added as a single end-of-run point.
    #[must_use]
    pub fn from_run(
        samples: &[MetricSample],
        series: &BTreeMap<String, Vec<MetricPoint>>,
        end_time_ms: u64,
    ) -> Self {
        let mut snapshot = Self::new(end_time_ms);
        for (key, points) in series {
            snapshot.insert(SeriesKey::parse(key), points.clone());
        }
        for sample in samples {
            let key = SeriesKey::new(sample.name.clone(), sample.labels.clone());
            if !snapshot.series.contains_key(&key) {
                snapshot.insert(
                    key,
                    vec![MetricPoint {
                        time_ms: end_time_ms,
                        value: sample.value.scalar(),
                    }],
                );
            }
        }
        snapshot
    }

    /// Add or replace one series' points, sorting them ascending in time.
    pub fn insert(&mut self, key: SeriesKey, mut points: Vec<MetricPoint>) {
        points.sort_by_key(|p| p.time_ms);
        self.series.insert(key, points);
    }

    /// Simulated time the run ended at, in milliseconds.
    #[must_use]
    pub fn end_time_ms(&self) -> u64 {
        self.end_time_ms
    }

    /// Whether the snapshot holds no series at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// The series matching `metric` and every matcher, in canonical key order.
    #[must_use]
    pub fn select(&self, metric: &str, matchers: &[(String, String)]) -> Vec<&SeriesKey> {
        self.series
            .keys()
            .filter(|key| key.matches(metric, matchers))
            .collect()
    }

    /// The points recorded for one series.
    #[must_use]
    pub fn points(&self, key: &SeriesKey) -> Option<&[MetricPoint]> {
        self.series.get(key).map(Vec::as_slice)
    }
}

// ---------------------------------------------------------------------------
// Aggregators
// ---------------------------------------------------------------------------

/// The four aggregations a query can apply, as a runtime value.
///
/// Queries name them through the marker types [`Min`], [`Mean`], [`Max`] and
/// [`Percentile`], which additionally carry what the result means for
/// percentile composition; this enum is what the compiled plan stores.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Aggregator {
    /// Smallest value.
    Min,
    /// Arithmetic mean.
    Mean,
    /// Largest value.
    Max,
    /// Percentile at the given rank in `[0.0, 1.0]`, linearly interpolated.
    Percentile(f64),
}

impl Aggregator {
    /// Apply to a non-empty set of values; `None` when `values` is empty.
    #[must_use]
    pub fn apply(self, values: &[f64]) -> Option<f64> {
        if values.is_empty() {
            return None;
        }
        Some(match self {
            Self::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
            Self::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            Self::Mean => mean(values),
            Self::Percentile(p) => {
                let mut sorted = values.to_vec();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                percentile_of_sorted(&sorted, p)
            }
        })
    }

    /// What applying this aggregator leaves behind.
    #[must_use]
    pub fn provenance(self) -> Provenance {
        match self {
            Self::Min | Self::Mean | Self::Max => Provenance::Scalar,
            Self::Percentile(_) => Provenance::Quantile,
        }
    }

    /// Short label for reports, e.g. `mean` or `p99`.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Min => "min".to_owned(),
            Self::Mean => "mean".to_owned(),
            Self::Max => "max".to_owned(),
            Self::Percentile(p) => format!("p{}", trim_float(p * 100.0)),
        }
    }
}

/// Arithmetic mean of a non-empty slice.
fn mean(values: &[f64]) -> f64 {
    let n = u32::try_from(values.len()).map_or(f64::INFINITY, f64::from);
    values.iter().sum::<f64>() / n
}

/// Percentile of an already-sorted slice, linearly interpolated between the
/// two neighbours of rank `p * (n - 1)`.
///
/// `p` is clamped to `[0.0, 1.0]`; a NaN rank reads as `0.0`.
fn percentile_of_sorted(sorted: &[f64], p: f64) -> f64 {
    debug_assert!(!sorted.is_empty(), "caller checked for emptiness");
    let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
    let last = sorted.len() - 1;
    let last_f = u32::try_from(last).map_or(f64::INFINITY, f64::from);
    let rank = p * last_f;
    let lower = rank.floor();
    let upper = rank.ceil();
    // SAFETY-adjacent: `rank` is finite and within `[0, last]`, so both bounds
    // index the slice. `f64 as usize` saturates, and `min(last)` pins it.
    let lower_idx = f64_to_index(lower).min(last);
    let upper_idx = f64_to_index(upper).min(last);
    if lower_idx == upper_idx {
        return sorted[lower_idx];
    }
    let weight = rank - lower;
    sorted[lower_idx] + (sorted[upper_idx] - sorted[lower_idx]) * weight
}

/// Convert a non-negative finite `f64` to an index, saturating at `usize::MAX`.
fn f64_to_index(v: f64) -> usize {
    if !v.is_finite() || v <= 0.0 {
        0
    } else {
        // Ranks are bounded by the slice length, so a plain cast is exact here.
        // `usize::try_from` on the u64 form keeps the conversion lint-clean.
        usize::try_from(f64_to_u64(v)).unwrap_or(usize::MAX)
    }
}

/// Truncate a non-negative finite `f64` to `u64`, saturating.
fn f64_to_u64(v: f64) -> u64 {
    const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;
    if !v.is_finite() || v <= 0.0 {
        0
    } else if v >= TWO_POW_64 {
        u64::MAX
    } else {
        // SAFETY: `v` is finite, non-negative and strictly below `2^64`.
        unsafe { v.to_int_unchecked::<u64>() }
    }
}

/// Render a float without a trailing `.0`, so `p99` beats `p99.0`.
fn trim_float(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

/// What to put in a bucket that no observation landed in.
///
/// [`bucketize`](MetricQuery::bucketize) omits empty buckets, because a gap
/// and a zero are different facts: a workload that stopped reporting is not a
/// workload reporting zero. [`fill`](MetricQuery::fill) is where you say which
/// of the two this metric means.
///
/// The names and the edge behaviour follow Warp 10's `FILLPREVIOUS`,
/// `FILLNEXT`, `FILLVALUE` and `INTERPOLATE`: carrying a value forward cannot
/// invent one before the series started, and carrying backward cannot invent
/// one after it ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fill {
    /// Carry the last known value forward. Leading gaps stay empty — there is
    /// nothing before the series to carry.
    Previous,
    /// Carry the next known value backward. Trailing gaps stay empty.
    Next,
    /// Linearly interpolate between the surrounding known values. Only
    /// interior gaps are filled: both neighbours must exist.
    Interpolate,
    /// A constant. `Fill::Value(0.0)` is the honest answer for a rate — no
    /// requests in that minute really is zero per second — and the only policy
    /// that fills leading and trailing gaps, since it needs no neighbour.
    Value(f64),
}

impl Fill {
    /// Short label for reports, e.g. `previous` or `0`.
    #[must_use]
    fn label(self) -> String {
        match self {
            Self::Previous => "previous".to_owned(),
            Self::Next => "next".to_owned(),
            Self::Interpolate => "interpolate".to_owned(),
            Self::Value(v) => trim_float(v),
        }
    }
}

// ---------------------------------------------------------------------------
// Stages and type-level percentile enforcement
// ---------------------------------------------------------------------------

mod sealed {
    /// Prevents downstream crates from adding stages or aggregators, which
    /// would let them route around the percentile rule.
    pub trait Sealed {}
}

/// What the values flowing through a query stage still represent.
///
/// Carried on [`MetricQueryPlan`] so the report can label a number honestly:
/// a [`Quantile`](Provenance::Quantile) result is a percentile, a
/// [`Scalar`](Provenance::Scalar) one is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Individual observations, or rates between consecutive ones. The
    /// distribution is intact, so a percentile over them is exact.
    Observations,
    /// Values collapsed by min, mean or max. The distribution is gone.
    Scalar,
    /// Values collapsed by a percentile. The distribution is gone, and the
    /// values are already order statistics of it.
    Quantile,
}

impl Provenance {
    /// Short label for reports.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Observations => "observations",
            Self::Scalar => "scalar",
            Self::Quantile => "percentile-derived",
        }
    }
}

/// A stage of a query pipeline, as a type. See [`Provenance`].
pub trait Stage: sealed::Sealed {
    /// What values at this stage represent.
    const PROVENANCE: Provenance;
}

/// Stage marker: values are individual observations — percentile-capable.
#[derive(Debug, Clone, Copy)]
pub struct Observations;

/// Stage marker: values were collapsed by min, mean or max.
#[derive(Debug, Clone, Copy)]
pub struct Scalar;

/// Stage marker: values were collapsed by a percentile.
#[derive(Debug, Clone, Copy)]
pub struct Quantile;

impl sealed::Sealed for Observations {}
impl sealed::Sealed for Scalar {}
impl sealed::Sealed for Quantile {}

impl Stage for Observations {
    const PROVENANCE: Provenance = Provenance::Observations;
}
impl Stage for Scalar {
    const PROVENANCE: Provenance = Provenance::Scalar;
}
impl Stage for Quantile {
    const PROVENANCE: Provenance = Provenance::Quantile;
}

/// Aggregator marker: smallest value.
#[derive(Debug, Clone, Copy)]
pub struct Min;

/// Aggregator marker: arithmetic mean.
#[derive(Debug, Clone, Copy)]
pub struct Mean;

/// Aggregator marker: largest value.
#[derive(Debug, Clone, Copy)]
pub struct Max;

/// Aggregator marker: percentile at rank `p` in `[0.0, 1.0]`.
///
/// Only implements [`ValidOn<Observations>`], which is what keeps
/// percentile-of-percentile from compiling.
#[derive(Debug, Clone, Copy)]
pub struct Percentile(pub f64);

impl sealed::Sealed for Min {}
impl sealed::Sealed for Mean {}
impl sealed::Sealed for Max {}
impl sealed::Sealed for Percentile {}

/// An aggregation the query builder accepts, and the stage it produces.
pub trait Aggregate: sealed::Sealed + Copy {
    /// The stage values are in after this aggregation.
    type Out: Stage;

    /// The runtime aggregator this marker compiles to.
    fn aggregator(self) -> Aggregator;
}

impl Aggregate for Min {
    type Out = Scalar;
    fn aggregator(self) -> Aggregator {
        Aggregator::Min
    }
}

impl Aggregate for Mean {
    type Out = Scalar;
    fn aggregator(self) -> Aggregator {
        Aggregator::Mean
    }
}

impl Aggregate for Max {
    type Out = Scalar;
    fn aggregator(self) -> Aggregator {
        Aggregator::Max
    }
}

impl Aggregate for Percentile {
    type Out = Quantile;
    fn aggregator(self) -> Aggregator {
        Aggregator::Percentile(self.0)
    }
}

/// Marker: aggregation `Self` is mathematically valid on values at stage `S`.
///
/// [`Min`], [`Mean`] and [`Max`] are valid everywhere. [`Percentile`] is valid
/// only on [`Observations`], because a percentile of already-collapsed values
/// is not a percentile of the distribution they came from.
pub trait ValidOn<S: Stage>: Aggregate {}

impl<S: Stage> ValidOn<S> for Min {}
impl<S: Stage> ValidOn<S> for Mean {}
impl<S: Stage> ValidOn<S> for Max {}
impl ValidOn<Observations> for Percentile {}

// ---------------------------------------------------------------------------
// Query builder
// ---------------------------------------------------------------------------

/// One step of a compiled query, applied in the order it was declared.
#[derive(Debug, Clone, PartialEq)]
enum QueryOp {
    Rate,
    Bucketize { every_ms: u64, agg: Aggregator },
    Map { window: usize, agg: Aggregator },
    Reduce { by: Option<String>, agg: Aggregator },
    Fill { policy: Fill },
}

/// A metric query under construction.
///
/// `S` tracks what the values currently are, which is what gates
/// [`Percentile`] — see the [module docs](self). Build with
/// [`select`](Self::select) and finish with [`named`](Self::named), which
/// erases the stage into a [`MetricQueryPlan`].
#[derive(Debug, Clone)]
pub struct MetricQuery<S: Stage = Observations> {
    metric: String,
    matchers: Vec<(String, String)>,
    ops: Vec<QueryOp>,
    // `fn() -> S` so the query stays `Send + Sync` whatever the marker is.
    stage: PhantomData<fn() -> S>,
}

impl MetricQuery<Observations> {
    /// Start a query selecting every series named `metric`.
    #[must_use]
    pub fn select(metric: impl Into<String>) -> Self {
        Self {
            metric: metric.into(),
            matchers: Vec::new(),
            ops: Vec::new(),
            stage: PhantomData,
        }
    }

    /// Narrow the selection to series carrying `key="value"`.
    ///
    /// Matching is by subset — a series may carry other labels — and repeating
    /// this method ANDs the matchers together.
    #[must_use]
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.matchers.push((key.into(), value.into()));
        self.matchers.sort();
        self
    }

    /// Read the selected series as monotonically increasing counters and turn
    /// them into a per-second rate on the simulated clock.
    ///
    /// Counter semantics are explicit here and nowhere else: no other stage
    /// infers them, so `bucketize(_, Mean)` on a raw counter honestly reports
    /// the mean *cumulative value*, not a rate.
    ///
    /// Each consecutive pair of points becomes one rate over the interval
    /// between them. A drop in value is read as a counter reset, Prometheus
    /// style: the new value is taken as the whole delta rather than producing
    /// a negative rate. Because the simulation builds a fresh metrics source
    /// per iteration, every counter is zero at simulated time zero, so an
    /// implicit origin point is prepended and the very first increment is
    /// rated too.
    #[must_use]
    pub fn rate(mut self) -> Self {
        self.ops.push(QueryOp::Rate);
        self
    }

    /// Regularize each series' points into fixed simulated-time buckets,
    /// aggregating the points that fall in each.
    ///
    /// A point belongs to bucket `n` when its timestamp is in
    /// `[n * every, (n + 1) * every)`; for a rate, the timestamp is the end of
    /// the interval the rate covers. Empty buckets are omitted rather than
    /// zero-filled — a gap is a gap, not a zero.
    ///
    /// Only available before any aggregation: bucketizing already-collapsed
    /// values would double-aggregate them.
    #[must_use]
    pub fn bucketize<A>(mut self, every: Duration, agg: A) -> MetricQuery<A::Out>
    where
        A: ValidOn<Observations>,
    {
        let every_ms = u64::try_from(every.as_millis()).unwrap_or(u64::MAX).max(1);
        self.ops.push(QueryOp::Bucketize {
            every_ms,
            agg: agg.aggregator(),
        });
        self.retag()
    }
}

impl<S: Stage> MetricQuery<S> {
    /// Re-tag the stage marker after an op that changes what values represent.
    fn retag<T: Stage>(self) -> MetricQuery<T> {
        MetricQuery {
            metric: self.metric,
            matchers: self.matchers,
            ops: self.ops,
            stage: PhantomData,
        }
    }

    /// Aggregate over a rolling window of `window` consecutive points of each
    /// series, one output per position where a full window is available.
    ///
    /// The output point spans from the first point of the window to the last,
    /// so a rolling mean over 60s buckets keeps bucket-aligned boundaries. A
    /// window of `0`, or one longer than the series, produces nothing.
    #[must_use]
    pub fn map<A>(mut self, window: usize, agg: A) -> MetricQuery<A::Out>
    where
        A: ValidOn<S>,
    {
        self.ops.push(QueryOp::Map {
            window,
            agg: agg.aggregator(),
        });
        self.retag()
    }

    /// Combine every selected series into one.
    ///
    /// After [`bucketize`](Self::bucketize) the series are aligned, so values
    /// are combined bucket by bucket and the buckets survive. Without it there
    /// is no shared time grid, so every value of every series is pooled into a
    /// single whole-run result ([`WHOLE_RUN_MS`] bounds) — which is how a
    /// global p99 across nodes is expressed.
    #[must_use]
    pub fn reduce<A>(mut self, agg: A) -> MetricQuery<A::Out>
    where
        A: ValidOn<S>,
    {
        self.ops.push(QueryOp::Reduce {
            by: None,
            agg: agg.aggregator(),
        });
        self.retag()
    }

    /// Combine series that share the same value of `label`, keeping one result
    /// series per distinct value.
    ///
    /// Alignment works as in [`reduce`](Self::reduce). Series that do not
    /// carry `label` at all are dropped — there is no group to put them in,
    /// which includes every series a previous reduce already collapsed, since
    /// those no longer carry the original label set.
    #[must_use]
    pub fn reduce_by<A>(mut self, label: impl Into<String>, agg: A) -> MetricQuery<A::Out>
    where
        A: ValidOn<S>,
    {
        self.ops.push(QueryOp::Reduce {
            by: Some(label.into()),
            agg: agg.aggregator(),
        });
        self.retag()
    }

    /// Give every empty bucket a value, so the series has no holes.
    ///
    /// Two things go wrong with holes. A gap reads as "no data" where the
    /// metric may well mean "zero", and — because a bucket set that depends on
    /// when a seed happened to be busy differs from seed to seed — the
    /// across-run summary splits one window into several, each with fewer runs
    /// in it. Filling puts every seed on the same grid.
    ///
    /// The grid runs from bucket zero to the bucket holding the run's end, so
    /// a workload that went quiet halfway through gets buckets for the silence
    /// rather than stopping early. Which of those get a value depends on the
    /// policy: see [`Fill`].
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use moonpool_core::metrics::query::{Fill, Mean, MetricQuery};
    /// MetricQuery::select("requests_total")
    ///     .rate()
    ///     .bucketize(Duration::from_secs(60), Mean)
    ///     // A minute with no requests is zero per second, not missing data.
    ///     .fill(Fill::Value(0.0))
    ///     .named("write_throughput");
    /// ```
    ///
    /// Filling neither collapses nor un-collapses anything, so the stage — and
    /// with it whether [`Percentile`] still applies — is unchanged. Without a
    /// preceding [`bucketize`](Self::bucketize) there is no grid and so no
    /// empty buckets to fill, and this does nothing.
    #[must_use]
    pub fn fill(mut self, policy: Fill) -> Self {
        self.ops.push(QueryOp::Fill { policy });
        self
    }

    /// Finish the query, naming it for the report.    /// Finish the query, naming it for the report.
    #[must_use]
    pub fn named(self, name: impl Into<String>) -> MetricQueryPlan {
        MetricQueryPlan {
            name: name.into(),
            metric: self.metric,
            matchers: self.matchers,
            ops: self.ops,
            provenance: S::PROVENANCE,
        }
    }
}

// ---------------------------------------------------------------------------
// Compiled plan
// ---------------------------------------------------------------------------

/// A finished, stage-erased query, ready to evaluate against any run.
///
/// Registered on the runner with `SimulationBuilder::metric` and evaluated
/// once per successful seed.
#[derive(Debug, Clone)]
pub struct MetricQueryPlan {
    name: String,
    metric: String,
    matchers: Vec<(String, String)>,
    ops: Vec<QueryOp>,
    provenance: Provenance,
}

impl MetricQueryPlan {
    /// The name this query reports under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The metric name this query selects.
    #[must_use]
    pub fn metric(&self) -> &str {
        &self.metric
    }

    /// What the query's values represent, for honest labelling.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        self.provenance
    }

    /// One-line rendering of the pipeline, e.g.
    /// `requests_total{operation="write"} → rate → 60s buckets (mean)`.
    #[must_use]
    pub fn description(&self) -> String {
        let selector = SeriesKey::new(self.metric.clone(), self.matchers.clone()).to_string();
        std::iter::once(selector)
            .chain(self.ops.iter().map(|op| match op {
                QueryOp::Rate => "rate".to_owned(),
                QueryOp::Bucketize { every_ms, agg } => {
                    format!("{} buckets ({})", fmt_millis(*every_ms), agg.label())
                }
                QueryOp::Map { window, agg } => format!("map {window} ({})", agg.label()),
                QueryOp::Reduce { by: None, agg } => format!("reduce ({})", agg.label()),
                QueryOp::Reduce {
                    by: Some(label),
                    agg,
                } => {
                    format!("reduce by {label} ({})", agg.label())
                }
                QueryOp::Fill { policy } => format!("fill ({})", policy.label()),
            }))
            .collect::<Vec<_>>()
            .join(" → ")
    }

    /// Evaluate against one run's snapshot.
    ///
    /// Rows come back in deterministic order — by group, then by window — and
    /// each carries the `run_id` and `seed` that produced it, so a bad value
    /// is replayable without any further bookkeeping.
    #[must_use]
    pub fn evaluate(
        &self,
        snapshot: &MetricSnapshot,
        run_id: u64,
        seed: u64,
    ) -> Vec<MetricQueryRow> {
        let mut state = EvalState::select(snapshot, &self.metric, &self.matchers);
        for op in &self.ops {
            state.apply(op);
        }
        let mut rows: Vec<MetricQueryRow> = state
            .series
            .into_iter()
            .flat_map(|series| {
                let group = series.group.clone();
                series.windows.into_iter().map(move |w| MetricQueryRow {
                    run_id,
                    seed,
                    query: self.name.clone(),
                    group: group.clone(),
                    bucket_start_ms: w.start_ms,
                    bucket_end_ms: w.end_ms,
                    value: w.value,
                })
            })
            .collect();
        rows.sort_by(row_order);
        rows
    }
}

/// Format a bucket width in the shortest exact unit, e.g. `60s` or `1500ms`.
fn fmt_millis(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Window bounds for a value that covers a whole run rather than a slice of
/// one. See [`MetricQueryRow::bucket_start_ms`].
pub const WHOLE_RUN_MS: u64 = 0;

/// One value covering a half-open simulated-time window `[start_ms, end_ms)`.
///
/// A raw observation has `start_ms == end_ms`: it happened at an instant.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Window {
    start_ms: u64,
    end_ms: u64,
    value: f64,
}

/// One logical series while a query runs.
#[derive(Debug, Clone)]
struct EvalSeries {
    /// `None` once the series has been reduced away into a single result.
    group: Option<String>,
    windows: Vec<Window>,
}

/// The working set a query pipeline transforms.
struct EvalState {
    series: Vec<EvalSeries>,
    /// Bucket width, once [`QueryOp::Bucketize`] has put every series on one
    /// grid. `Some` is also what makes a bucket-by-bucket
    /// [`QueryOp::Reduce`] meaningful and what [`QueryOp::Fill`] needs to know
    /// where the empty buckets are.
    grid: Option<u64>,
    /// Simulated time the run ended at, which bounds the grid a fill spans.
    end_time_ms: u64,
}

impl EvalState {
    fn select(snapshot: &MetricSnapshot, metric: &str, matchers: &[(String, String)]) -> Self {
        let series = snapshot
            .select(metric, matchers)
            .into_iter()
            .map(|key| EvalSeries {
                group: Some(key.to_string()),
                windows: snapshot
                    .points(key)
                    .unwrap_or_default()
                    .iter()
                    .map(|p| Window {
                        start_ms: p.time_ms,
                        end_ms: p.time_ms,
                        value: p.value,
                    })
                    .collect(),
            })
            .collect();
        Self {
            series,
            grid: None,
            end_time_ms: snapshot.end_time_ms(),
        }
    }

    fn apply(&mut self, op: &QueryOp) {
        match op {
            QueryOp::Rate => {
                for s in &mut self.series {
                    s.windows = rate(&s.windows);
                }
                self.grid = None;
            }
            QueryOp::Bucketize { every_ms, agg } => {
                for s in &mut self.series {
                    s.windows = bucketize(&s.windows, *every_ms, *agg);
                }
                self.grid = Some(*every_ms);
            }
            QueryOp::Map { window, agg } => {
                for s in &mut self.series {
                    s.windows = rolling_map(&s.windows, *window, *agg);
                }
            }
            QueryOp::Reduce { by, agg } => self.reduce(by.as_deref(), *agg),
            QueryOp::Fill { policy } => {
                // Without a grid there are no empty buckets to fill.
                if let Some(every_ms) = self.grid {
                    for s in &mut self.series {
                        s.windows = fill_gaps(&s.windows, every_ms, self.end_time_ms, *policy);
                    }
                }
            }
        }
        self.series.retain(|s| !s.windows.is_empty());
    }

    fn reduce(&mut self, by: Option<&str>, agg: Aggregator) {
        // Group series: everything into one, or one group per label value.
        let mut groups: BTreeMap<Option<String>, Vec<&EvalSeries>> = BTreeMap::new();
        for s in &self.series {
            let group = match by {
                None => None,
                Some(label) => {
                    let Some(key) = s.group.as_ref().map(|g| SeriesKey::parse(g)) else {
                        continue;
                    };
                    let Some(value) = key.label(label) else {
                        continue;
                    };
                    Some(value.to_owned())
                }
            };
            groups.entry(group).or_default().push(s);
        }

        let aligned = self.grid.is_some();
        self.series = groups
            .into_iter()
            .map(|(group, members)| EvalSeries {
                group,
                windows: combine(&members, aligned, agg),
            })
            .collect();
    }
}

/// Combine several series into one, bucket by bucket when they share a grid
/// and by pooling every value otherwise.
fn combine(members: &[&EvalSeries], aligned: bool, agg: Aggregator) -> Vec<Window> {
    if aligned {
        let mut windows: BTreeSet<(u64, u64)> = BTreeSet::new();
        for s in members {
            for w in &s.windows {
                windows.insert((w.start_ms, w.end_ms));
            }
        }
        return windows
            .into_iter()
            .filter_map(|(start_ms, end_ms)| {
                let values: Vec<f64> = members
                    .iter()
                    .flat_map(|s| s.windows.iter())
                    .filter(|w| w.start_ms == start_ms && w.end_ms == end_ms)
                    .map(|w| w.value)
                    .collect();
                agg.apply(&values).map(|value| Window {
                    start_ms,
                    end_ms,
                    value,
                })
            })
            .collect();
    }

    let values: Vec<f64> = members
        .iter()
        .flat_map(|s| s.windows.iter())
        .map(|w| w.value)
        .collect();
    // A pooled reduce covers the whole run, so it gets the whole-run window
    // rather than the span its observations happened to occupy. Those spans
    // differ from seed to seed, and stamping them here would split the
    // across-run summary into one single-run window per seed.
    agg.apply(&values)
        .map(|value| {
            vec![Window {
                start_ms: WHOLE_RUN_MS,
                end_ms: WHOLE_RUN_MS,
                value,
            }]
        })
        .unwrap_or_default()
}

/// Per-second rate between consecutive counter readings.
///
/// Readings at the same instant are coalesced to the last one first, so a
/// burst of increments inside one simulated millisecond does not divide by
/// zero. A decrease is read as a counter reset.
fn rate(windows: &[Window]) -> Vec<Window> {
    if windows.is_empty() {
        return Vec::new();
    }

    // Coalesce same-instant readings, keeping the final value.
    let mut readings: Vec<(u64, f64)> = Vec::with_capacity(windows.len() + 1);
    // A fresh metrics source starts every counter at zero at simulated time
    // zero, so the first increment has a real interval to be rated over.
    if windows[0].end_ms > 0 {
        readings.push((0, 0.0));
    }
    for w in windows {
        match readings.last_mut() {
            Some((t, v)) if *t == w.end_ms => *v = w.value,
            _ => readings.push((w.end_ms, w.value)),
        }
    }

    readings
        .windows(2)
        .filter_map(|pair| {
            let (t0, v0) = pair[0];
            let (t1, v1) = pair[1];
            let elapsed_ms = t1.checked_sub(t0)?;
            if elapsed_ms == 0 {
                return None;
            }
            // Prometheus' reset rule: a drop means the counter restarted, so
            // the new value is the whole delta rather than a negative rate.
            let delta = if v1 >= v0 { v1 - v0 } else { v1 };
            let seconds = u32::try_from(elapsed_ms).map_or(f64::INFINITY, f64::from) / 1000.0;
            Some(Window {
                start_ms: t0,
                end_ms: t1,
                value: delta / seconds,
            })
        })
        .collect()
}

/// Aggregate points into fixed-width buckets keyed off their end timestamp.
fn bucketize(windows: &[Window], every_ms: u64, agg: Aggregator) -> Vec<Window> {
    let mut buckets: BTreeMap<u64, Vec<f64>> = BTreeMap::new();
    for w in windows {
        buckets
            .entry(w.end_ms / every_ms)
            .or_default()
            .push(w.value);
    }
    buckets
        .into_iter()
        .filter_map(|(index, values)| {
            let start_ms = index.saturating_mul(every_ms);
            agg.apply(&values).map(|value| Window {
                start_ms,
                end_ms: start_ms.saturating_add(every_ms),
                value,
            })
        })
        .collect()
}

/// Give every empty bucket in the grid a value, per `policy`.
///
/// The grid runs from bucket zero to whichever is later: the bucket holding
/// the run's end, or the last bucket that actually has data. Bucket zero
/// rather than the series' first bucket, so two seeds that started producing
/// at different moments still land on the same windows.
fn fill_gaps(windows: &[Window], every_ms: u64, end_time_ms: u64, policy: Fill) -> Vec<Window> {
    let known: BTreeMap<u64, f64> = windows
        .iter()
        .map(|w| (w.start_ms / every_ms, w.value))
        .collect();
    let Some(last_known) = known.keys().next_back().copied() else {
        // Nothing was ever recorded: there is no series to give holes to, and
        // inventing a whole run of values out of a policy alone would be
        // reporting on a metric the run never touched.
        return Vec::new();
    };
    let last_index = (end_time_ms / every_ms).max(last_known);

    (0..=last_index)
        .filter_map(|index| {
            let value = match known.get(&index) {
                Some(value) => Some(*value),
                None => fill_value(&known, index, policy),
            }?;
            let start_ms = index.saturating_mul(every_ms);
            Some(Window {
                start_ms,
                end_ms: start_ms.saturating_add(every_ms),
                value,
            })
        })
        .collect()
}

/// The value `policy` puts in empty bucket `index`, or `None` when the policy
/// has no neighbour to work from — a leading gap for [`Fill::Previous`], a
/// trailing one for [`Fill::Next`], either for [`Fill::Interpolate`].
///
/// Neighbours are always looked up among the *recorded* buckets, never among
/// already-filled ones, so carrying a value forward carries the last real
/// observation rather than a copy of a copy.
fn fill_value(known: &BTreeMap<u64, f64>, index: u64, policy: Fill) -> Option<f64> {
    let previous = || known.range(..index).next_back().map(|(i, v)| (*i, *v));
    let next = || known.range(index..).next().map(|(i, v)| (*i, *v));
    match policy {
        Fill::Value(value) => Some(value),
        Fill::Previous => previous().map(|(_, v)| v),
        Fill::Next => next().map(|(_, v)| v),
        Fill::Interpolate => {
            let (before, before_value) = previous()?;
            let (after, after_value) = next()?;
            let span = u32::try_from(after - before).map_or(f64::INFINITY, f64::from);
            let offset = u32::try_from(index - before).map_or(f64::INFINITY, f64::from);
            Some(before_value + (after_value - before_value) * (offset / span))
        }
    }
}

/// Trailing rolling window of `window` consecutive points.
fn rolling_map(windows: &[Window], window: usize, agg: Aggregator) -> Vec<Window> {
    if window == 0 || window > windows.len() {
        return Vec::new();
    }
    windows
        .windows(window)
        .filter_map(|slice| {
            let values: Vec<f64> = slice.iter().map(|w| w.value).collect();
            let last = slice.last()?;
            let first = slice.first()?;
            agg.apply(&values).map(|value| Window {
                start_ms: first.start_ms,
                end_ms: last.end_ms,
                value,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// One evaluated value of one query, in one run.
///
/// Deliberately flat: this is the row a CSV export writes, and the shape that
/// makes "which seed produced this?" answerable without a join.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricQueryRow {
    /// Identity of the whole exploration invocation that produced this row.
    pub run_id: u64,
    /// The seed of the iteration that produced it — replay this to get it back.
    pub seed: u64,
    /// The name the query was registered under.
    pub query: String,
    /// Series or group identity: the series key before any reduce, the label
    /// value after [`reduce_by`](MetricQuery::reduce_by), and `None` once the
    /// query has reduced everything into one series.
    pub group: Option<String>,
    /// Start of the simulated-time window this value covers, in milliseconds.
    ///
    /// A value that covers the whole run rather than a slice of one — what a
    /// [`reduce`](MetricQuery::reduce) without
    /// [`bucketize`](MetricQuery::bucketize) produces — reports
    /// [`WHOLE_RUN_MS`] for both bounds, so every seed's value lands in the
    /// same across-run summary instead of in a window of its own.
    pub bucket_start_ms: u64,
    /// End of that window, exclusive. Equal to the start for an instant value
    /// and for a whole-run value.
    pub bucket_end_ms: u64,
    /// The value.
    pub value: f64,
}

/// Deterministic row order: group, then window, then seed.
fn row_order(a: &MetricQueryRow, b: &MetricQueryRow) -> std::cmp::Ordering {
    a.group
        .cmp(&b.group)
        .then(a.bucket_start_ms.cmp(&b.bucket_start_ms))
        .then(a.bucket_end_ms.cmp(&b.bucket_end_ms))
        .then(a.seed.cmp(&b.seed))
}

/// One query's values for one window, summarized across every run.
///
/// The percentiles here are over *runs*: each seed contributes one value, so
/// `p95` names the 95th-percentile run, not a percentile of the underlying
/// observations. That stays valid whatever the query's own
/// [`Provenance`] is, because it aggregates a different dimension.
#[derive(Debug, Clone)]
pub struct MetricWindowSummary {
    /// Series or group identity; see [`MetricQueryRow::group`].
    pub group: Option<String>,
    /// Start of the window, in simulated milliseconds.
    pub bucket_start_ms: u64,
    /// End of the window, exclusive.
    pub bucket_end_ms: u64,
    /// Number of distinct seeds that contributed a value.
    pub runs: usize,
    /// Smallest value seen in any run.
    pub min: f64,
    /// Seed that produced [`min`](Self::min) — the smallest such seed on ties.
    pub min_seed: u64,
    /// Largest value seen in any run.
    pub max: f64,
    /// Seed that produced [`max`](Self::max) — the smallest such seed on ties.
    pub max_seed: u64,
    /// Mean across runs.
    pub mean: f64,
    /// Median run.
    pub p50: f64,
    /// 95th-percentile run.
    pub p95: f64,
    /// 99th-percentile run.
    pub p99: f64,
}

/// Every evaluated row of one named query, plus its across-run summary.
///
/// This is what the end-of-exploration report prints, and what a future CSV
/// export will serialize — [`rows`](Self::rows) is already the row set.
#[derive(Debug, Clone)]
pub struct MetricQueryReport {
    /// The query's registered name.
    pub name: String,
    /// One-line rendering of the pipeline; see [`MetricQueryPlan::description`].
    pub description: String,
    /// What each run's values represent.
    pub provenance: Provenance,
    /// Number of distinct seeds that contributed at least one row.
    pub runs: usize,
    /// Every row, in deterministic order.
    pub rows: Vec<MetricQueryRow>,
    /// Across-run summary, one entry per (group, window), in the same order.
    pub windows: Vec<MetricWindowSummary>,
}

impl MetricQueryReport {
    /// Summarize a query's rows across every run that produced them.
    #[must_use]
    pub fn from_rows(plan: &MetricQueryPlan, mut rows: Vec<MetricQueryRow>) -> Self {
        rows.sort_by(row_order);

        let runs = rows.iter().map(|r| r.seed).collect::<BTreeSet<_>>().len();

        // Rows are sorted by (group, window), so equal keys are contiguous.
        let mut windows = Vec::new();
        let mut start = 0usize;
        while start < rows.len() {
            let head = &rows[start];
            let mut end = start + 1;
            while end < rows.len()
                && rows[end].group == head.group
                && rows[end].bucket_start_ms == head.bucket_start_ms
                && rows[end].bucket_end_ms == head.bucket_end_ms
            {
                end += 1;
            }
            windows.push(summarize_window(&rows[start..end]));
            start = end;
        }

        Self {
            name: plan.name.clone(),
            description: plan.description(),
            provenance: plan.provenance,
            runs,
            rows,
            windows,
        }
    }
}

/// Summarize one contiguous run of rows sharing a (group, window) key.
fn summarize_window(rows: &[MetricQueryRow]) -> MetricWindowSummary {
    debug_assert!(!rows.is_empty(), "caller groups non-empty slices");
    let head = &rows[0];

    // Rows are seed-ordered within a window, so the first extremum wins ties
    // with the smallest seed — which keeps the reported seed deterministic.
    let mut min = f64::INFINITY;
    let mut min_seed = head.seed;
    let mut max = f64::NEG_INFINITY;
    let mut max_seed = head.seed;
    for row in rows {
        if row.value < min {
            min = row.value;
            min_seed = row.seed;
        }
        if row.value > max {
            max = row.value;
            max_seed = row.seed;
        }
    }

    let mut values: Vec<f64> = rows.iter().map(|r| r.value).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    MetricWindowSummary {
        group: head.group.clone(),
        bucket_start_ms: head.bucket_start_ms,
        bucket_end_ms: head.bucket_end_ms,
        runs: rows.iter().map(|r| r.seed).collect::<BTreeSet<_>>().len(),
        min,
        min_seed,
        max,
        max_seed,
        mean: mean(&values),
        p50: percentile_of_sorted(&values, 0.50),
        p95: percentile_of_sorted(&values, 0.95),
        p99: percentile_of_sorted(&values, 0.99),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricValue;

    /// Floating-point comparison for the expected values in these tests, which
    /// are small and exactly representable up to accumulated rounding.
    fn is_close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn snapshot(series: &[(&str, &[(u64, f64)])], end_time_ms: u64) -> MetricSnapshot {
        let mut snap = MetricSnapshot::new(end_time_ms);
        for (key, points) in series {
            snap.insert(
                SeriesKey::parse(key),
                points
                    .iter()
                    .map(|(time_ms, value)| MetricPoint {
                        time_ms: *time_ms,
                        value: *value,
                    })
                    .collect(),
            );
        }
        snap
    }

    // --- series identity -------------------------------------------------

    #[test]
    fn labels_are_canonically_ordered() {
        let a = SeriesKey::new(
            "requests_total",
            vec![
                ("method".to_owned(), "GET".to_owned()),
                ("code".to_owned(), "200".to_owned()),
            ],
        );
        let b = SeriesKey::new(
            "requests_total",
            vec![
                ("code".to_owned(), "200".to_owned()),
                ("method".to_owned(), "GET".to_owned()),
            ],
        );
        assert_eq!(a, b, "insertion order must not change series identity");
        assert_eq!(a.to_string(), r#"requests_total{code="200",method="GET"}"#);
    }

    #[test]
    fn parse_round_trips_the_canonical_form() {
        let key = SeriesKey::parse(r#"requests_total{method="GET",code="200"}"#);
        assert_eq!(key.metric, "requests_total");
        assert_eq!(key.label("code"), Some("200"));
        assert_eq!(
            key.to_string(),
            r#"requests_total{code="200",method="GET"}"#
        );

        let bare = SeriesKey::parse("uptime");
        assert_eq!(bare.metric, "uptime");
        assert!(bare.labels.is_empty());
        assert_eq!(bare.to_string(), "uptime");
    }

    #[test]
    fn a_sample_and_its_recorded_series_agree_on_identity() {
        let sample = MetricSample::new(
            "requests_total",
            vec![
                ("op".to_owned(), "write".to_owned()),
                ("instance".to_owned(), "10.0.1.1".to_owned()),
            ],
            MetricValue::Counter(1.0),
        );
        assert_eq!(
            SeriesKey::parse(&sample.sort_key()),
            SeriesKey::new(sample.name.clone(), sample.labels.clone()),
        );
    }

    // --- selection -------------------------------------------------------

    #[test]
    fn selection_is_by_name_then_label_subset() {
        let snap = snapshot(
            &[
                (r#"requests_total{instance="a",op="write"}"#, &[(0, 1.0)]),
                (r#"requests_total{instance="b",op="read"}"#, &[(0, 2.0)]),
                (r#"other_total{op="write"}"#, &[(0, 3.0)]),
            ],
            1_000,
        );

        let all = snap.select("requests_total", &[]);
        assert_eq!(all.len(), 2, "name match only");

        let writes = snap.select("requests_total", &[("op".to_owned(), "write".to_owned())]);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].label("instance"), Some("a"));

        let none = snap.select("requests_total", &[("op".to_owned(), "purge".to_owned())]);
        assert!(none.is_empty(), "no series carries that label value");

        let subset = snap.select("requests_total", &[("instance".to_owned(), "a".to_owned())]);
        assert_eq!(subset.len(), 1, "extra labels on the series are fine");
    }

    #[test]
    fn a_scrape_only_series_becomes_one_end_of_run_point() {
        let samples = vec![MetricSample::new(
            "foreign_total",
            Vec::new(),
            MetricValue::Counter(50.0),
        )];
        let snap = MetricSnapshot::from_run(&samples, &BTreeMap::new(), 10_000);
        let key = SeriesKey::parse("foreign_total");
        let points = snap.points(&key).expect("scraped series is selectable");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].time_ms, 10_000);

        // And rate() reads it as "50 over the whole run" = 5/s.
        let rows = MetricQuery::select("foreign_total")
            .rate()
            .reduce(Mean)
            .named("foreign_rate")
            .evaluate(&snap, 7, 42);
        assert_eq!(rows.len(), 1);
        assert!(is_close(rows[0].value, 5.0));
    }

    #[test]
    fn a_recorded_series_wins_over_the_scrape() {
        let samples = vec![MetricSample::new(
            "hits_total",
            Vec::new(),
            MetricValue::Counter(9.0),
        )];
        let mut series = BTreeMap::new();
        series.insert(
            "hits_total".to_owned(),
            vec![
                MetricPoint {
                    time_ms: 1_000,
                    value: 1.0,
                },
                MetricPoint {
                    time_ms: 2_000,
                    value: 2.0,
                },
            ],
        );
        let snap = MetricSnapshot::from_run(&samples, &series, 3_000);
        let points = snap
            .points(&SeriesKey::parse("hits_total"))
            .expect("recorded");
        assert_eq!(points.len(), 2, "history beats the single scrape value");
    }

    // --- rate ------------------------------------------------------------

    #[test]
    fn rate_divides_by_simulated_time() {
        // 10 increments over 1s starting from an implicit zero at t=0.
        let snap = snapshot(&[("hits_total", &[(1_000, 10.0), (3_000, 30.0)])], 3_000);
        let rows = MetricQuery::select("hits_total")
            .rate()
            .named("r")
            .evaluate(&snap, 1, 1);

        assert_eq!(rows.len(), 2, "origin point makes the first interval real");
        assert_eq!((rows[0].bucket_start_ms, rows[0].bucket_end_ms), (0, 1_000));
        assert!(is_close(rows[0].value, 10.0), "10 over 1s");
        assert_eq!(
            (rows[1].bucket_start_ms, rows[1].bucket_end_ms),
            (1_000, 3_000)
        );
        assert!(is_close(rows[1].value, 10.0), "20 over 2s");
    }

    #[test]
    fn rate_coalesces_same_instant_readings() {
        // Three increments inside the same simulated millisecond: the counter
        // reads 3 at t=1000, and nothing divides by a zero interval.
        let snap = snapshot(
            &[("hits_total", &[(1_000, 1.0), (1_000, 2.0), (1_000, 3.0)])],
            1_000,
        );
        let rows = MetricQuery::select("hits_total")
            .rate()
            .named("r")
            .evaluate(&snap, 1, 1);
        assert_eq!(rows.len(), 1);
        assert!(is_close(rows[0].value, 3.0));
    }

    #[test]
    fn rate_reads_a_drop_as_a_counter_reset() {
        // 0 → 10 → (reset) → 4: the last interval contributes 4, not -6.
        let snap = snapshot(&[("hits_total", &[(1_000, 10.0), (2_000, 4.0)])], 2_000);
        let rows = MetricQuery::select("hits_total")
            .rate()
            .named("r")
            .evaluate(&snap, 1, 1);
        assert_eq!(rows.len(), 2);
        assert!(is_close(rows[1].value, 4.0), "reset, not a negative rate");
    }

    #[test]
    fn bucketize_alone_does_not_infer_counter_semantics() {
        // Same counter, no rate(): the mean of the cumulative readings, which
        // is honestly not a throughput.
        let snap = snapshot(&[("hits_total", &[(1_000, 10.0), (3_000, 30.0)])], 3_000);
        let rows = MetricQuery::select("hits_total")
            .bucketize(Duration::from_mins(1), Mean)
            .named("m")
            .evaluate(&snap, 1, 1);
        assert_eq!(rows.len(), 1);
        assert!(is_close(rows[0].value, 20.0));
    }

    // --- bucketize -------------------------------------------------------

    #[test]
    fn bucket_boundaries_are_half_open() {
        let snap = snapshot(
            &[(
                "latency",
                &[
                    (0, 1.0),
                    (59_999, 3.0),
                    (60_000, 100.0),
                    (119_999, 200.0),
                    (180_000, 7.0),
                ],
            )],
            180_000,
        );
        let rows = MetricQuery::select("latency")
            .bucketize(Duration::from_mins(1), Mean)
            .named("b")
            .evaluate(&snap, 1, 1);

        assert_eq!(rows.len(), 3, "the empty 120-180s bucket is omitted");
        assert_eq!(
            (rows[0].bucket_start_ms, rows[0].bucket_end_ms),
            (0, 60_000)
        );
        assert!(is_close(rows[0].value, 2.0), "1 and 3, not 100");
        assert_eq!(
            (rows[1].bucket_start_ms, rows[1].bucket_end_ms),
            (60_000, 120_000)
        );
        assert!(is_close(rows[1].value, 150.0));
        assert_eq!(
            (rows[2].bucket_start_ms, rows[2].bucket_end_ms),
            (180_000, 240_000)
        );
    }

    #[test]
    fn bucketize_supports_min_mean_max_and_percentile() {
        let points: Vec<(u64, f64)> = (1u32..=100).map(|i| (u64::from(i), f64::from(i))).collect();
        let snap = snapshot(&[("v", points.as_slice())], 100);
        let value = |agg: Aggregator| {
            let plan = match agg {
                Aggregator::Min => MetricQuery::select("v")
                    .bucketize(Duration::from_mins(1), Min)
                    .named("q"),
                Aggregator::Mean => MetricQuery::select("v")
                    .bucketize(Duration::from_mins(1), Mean)
                    .named("q"),
                Aggregator::Max => MetricQuery::select("v")
                    .bucketize(Duration::from_mins(1), Max)
                    .named("q"),
                Aggregator::Percentile(p) => MetricQuery::select("v")
                    .bucketize(Duration::from_mins(1), Percentile(p))
                    .named("q"),
            };
            plan.evaluate(&snap, 1, 1)[0].value
        };

        assert!(is_close(value(Aggregator::Min), 1.0));
        assert!(is_close(value(Aggregator::Max), 100.0));
        assert!(is_close(value(Aggregator::Mean), 50.5));
        // Linear interpolation on rank p*(n-1): 0.99 * 99 = 98.01.
        assert!(is_close(value(Aggregator::Percentile(0.99)), 99.01));
        assert!(is_close(value(Aggregator::Percentile(0.5)), 50.5));
    }

    // --- fill ------------------------------------------------------------

    /// Buckets 0 and 3 have data; 1, 2 and 4 are empty. The run ends inside
    /// bucket 4, so the grid is 0..=4.
    fn gapped() -> MetricSnapshot {
        snapshot(&[("v", &[(0, 10.0), (180_000, 40.0)])], 240_000)
    }

    fn filled(policy: Fill) -> Vec<(u64, f64)> {
        MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .fill(policy)
            .named("q")
            .evaluate(&gapped(), 1, 1)
            .into_iter()
            .map(|row| (row.bucket_start_ms / 60_000, row.value))
            .collect()
    }

    #[test]
    fn without_fill_a_gap_stays_a_gap() {
        let rows = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .named("q")
            .evaluate(&gapped(), 1, 1);
        assert_eq!(
            rows.iter()
                .map(|r| r.bucket_start_ms / 60_000)
                .collect::<Vec<_>>(),
            vec![0, 3],
            "empty buckets are omitted unless a fill policy asks otherwise"
        );
    }

    #[test]
    fn fill_previous_carries_forward_but_invents_no_prologue() {
        assert_eq!(
            filled(Fill::Previous),
            vec![(0, 10.0), (1, 10.0), (2, 10.0), (3, 40.0), (4, 40.0)],
            "interior and trailing gaps take the last known value"
        );

        // Bucket 0 empty: nothing precedes it, so it stays empty.
        let late = snapshot(&[("v", &[(120_000, 7.0)])], 180_000);
        let rows = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .fill(Fill::Previous)
            .named("q")
            .evaluate(&late, 1, 1);
        assert_eq!(
            rows.iter()
                .map(|r| r.bucket_start_ms / 60_000)
                .collect::<Vec<_>>(),
            vec![2, 3],
            "a leading gap has nothing to carry forward"
        );
    }

    #[test]
    fn fill_next_carries_backward_but_invents_no_epilogue() {
        assert_eq!(
            filled(Fill::Next),
            vec![(0, 10.0), (1, 40.0), (2, 40.0), (3, 40.0)],
            "interior gaps take the next known value; the trailing one stays empty"
        );
    }

    #[test]
    fn fill_value_covers_the_whole_grid() {
        assert_eq!(
            filled(Fill::Value(0.0)),
            vec![(0, 10.0), (1, 0.0), (2, 0.0), (3, 40.0), (4, 0.0)],
            "a constant needs no neighbour, so leading and trailing gaps fill too"
        );
    }

    #[test]
    fn interpolate_fills_only_between_known_values() {
        assert_eq!(
            filled(Fill::Interpolate),
            vec![(0, 10.0), (1, 20.0), (2, 30.0), (3, 40.0)],
            "linear between bucket 0 and bucket 3; nothing to interpolate after"
        );
    }

    #[test]
    fn interpolation_walks_the_recorded_neighbours_not_the_filled_ones() {
        // 0 → 100 over four buckets must step by 25, which it only does if
        // each gap looks back at bucket 0 rather than at the value just
        // synthesized for the bucket before it.
        let snap = snapshot(&[("v", &[(0, 0.0), (240_000, 100.0)])], 240_000);
        let values: Vec<f64> = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .fill(Fill::Interpolate)
            .named("q")
            .evaluate(&snap, 1, 1)
            .into_iter()
            .map(|row| row.value)
            .collect();
        assert_eq!(values.len(), 5);
        for (i, value) in values.iter().enumerate() {
            let expected = 25.0 * u32::try_from(i).map_or(f64::INFINITY, f64::from);
            assert!(is_close(*value, expected), "bucket {i}: {value}");
        }
    }

    #[test]
    fn fill_extends_the_grid_to_the_end_of_the_run() {
        // All the data is in the first minute, but the run lasted five.
        let snap = snapshot(&[("v", &[(0, 3.0)])], 300_000);
        let rows = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .fill(Fill::Value(0.0))
            .named("q")
            .evaluate(&snap, 1, 1);
        assert_eq!(rows.len(), 6, "the silence gets buckets too");
        assert!(is_close(rows[5].value, 0.0));
        assert_eq!(rows[5].bucket_start_ms, 300_000);
    }

    #[test]
    fn filling_a_series_that_never_reported_stays_empty() {
        // No observations at all: there is nothing to have holes in, and a
        // policy alone must not conjure a series the run never touched.
        let snap = snapshot(&[("other", &[(0, 1.0)])], 120_000);
        let rows = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .fill(Fill::Value(0.0))
            .named("q")
            .evaluate(&snap, 1, 1);
        assert!(rows.is_empty());
    }

    #[test]
    fn fill_without_bucketize_does_nothing() {
        let snap = snapshot(&[("v", &[(0, 1.0), (5_000, 2.0)])], 10_000);
        let rows = MetricQuery::select("v")
            .fill(Fill::Value(0.0))
            .named("q")
            .evaluate(&snap, 1, 1);
        assert_eq!(rows.len(), 2, "no grid means no empty buckets to fill");
    }

    #[test]
    fn fill_preserves_the_stage_so_percentiles_stay_gated() {
        let quantile = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Percentile(0.99))
            .fill(Fill::Previous)
            .named("q");
        assert_eq!(
            quantile.provenance(),
            Provenance::Quantile,
            "filling neither collapses nor restores a distribution"
        );
        assert_eq!(
            quantile.description(),
            "v → 60s buckets (p99) → fill (previous)"
        );
    }

    #[test]
    fn filling_puts_every_seed_on_the_same_windows() {
        let plan = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .fill(Fill::Value(0.0))
            .named("q");
        // Two seeds busy in different minutes of a three-minute run.
        let rows: Vec<MetricQueryRow> = [(1u64, 0u64), (2, 120_000)]
            .iter()
            .flat_map(|(seed, at_ms)| {
                let snap = snapshot(&[("v", &[(*at_ms, 5.0)])], 180_000);
                plan.evaluate(&snap, 7, *seed)
            })
            .collect();
        let report = MetricQueryReport::from_rows(&plan, rows);

        assert_eq!(report.windows.len(), 4, "one window per bucket of the grid");
        assert!(
            report.windows.iter().all(|w| w.runs == 2),
            "every window has both runs in it, so min/max compare like for like"
        );
        // Bucket 0: seed 1 saw 5, seed 2 saw nothing (filled to 0).
        assert!(is_close(report.windows[0].min, 0.0));
        assert_eq!(report.windows[0].min_seed, 2);
        assert!(is_close(report.windows[0].max, 5.0));
        assert_eq!(report.windows[0].max_seed, 1);
    }

    #[test]
    fn unfilled_windows_fragment_the_summary() {
        // The same two seeds without a fill: each bucket has one run in it,
        // which is exactly the reporting problem fill exists to solve.
        let plan = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .named("q");
        let rows: Vec<MetricQueryRow> = [(1u64, 0u64), (2, 120_000)]
            .iter()
            .flat_map(|(seed, at_ms)| {
                let snap = snapshot(&[("v", &[(*at_ms, 5.0)])], 180_000);
                plan.evaluate(&snap, 7, *seed)
            })
            .collect();
        let report = MetricQueryReport::from_rows(&plan, rows);
        assert_eq!(report.windows.len(), 2);
        assert!(report.windows.iter().all(|w| w.runs == 1));
    }

    // --- map -------------------------------------------------------------

    #[test]
    fn map_rolls_a_window_over_consecutive_points() {
        let snap = snapshot(
            &[("v", &[(0, 1.0), (1_000, 5.0), (2_000, 3.0), (3_000, 9.0)])],
            3_000,
        );
        let rows = MetricQuery::select("v")
            .map(2, Max)
            .named("rolling_max")
            .evaluate(&snap, 1, 1);

        assert_eq!(rows.len(), 3, "one per full window");
        assert!(is_close(rows[0].value, 5.0));
        assert!(is_close(rows[1].value, 5.0));
        assert!(is_close(rows[2].value, 9.0));
        assert_eq!(
            (rows[0].bucket_start_ms, rows[0].bucket_end_ms),
            (0, 1_000),
            "the window spans its first point to its last"
        );
    }

    #[test]
    fn map_over_buckets_keeps_bucket_boundaries() {
        let snap = snapshot(
            &[("v", &[(0, 1.0), (60_000, 2.0), (120_000, 30.0)])],
            180_000,
        );
        let rows = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .map(2, Mean)
            .named("smoothed")
            .evaluate(&snap, 1, 1);

        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].bucket_start_ms, rows[0].bucket_end_ms),
            (0, 120_000)
        );
        assert!(is_close(rows[0].value, 1.5));
        assert!(is_close(rows[1].value, 16.0));
    }

    #[test]
    fn a_window_longer_than_the_series_produces_nothing() {
        let snap = snapshot(&[("v", &[(0, 1.0)])], 0);
        let rows = MetricQuery::select("v")
            .map(5, Mean)
            .named("q")
            .evaluate(&snap, 1, 1);
        assert!(rows.is_empty());
    }

    // --- reduce ----------------------------------------------------------

    #[test]
    fn reduce_combines_aligned_series_bucket_by_bucket() {
        let snap = snapshot(
            &[
                (r#"v{instance="a"}"#, &[(0, 10.0), (60_000, 100.0)]),
                (r#"v{instance="b"}"#, &[(0, 20.0), (60_000, 200.0)]),
            ],
            120_000,
        );
        let rows = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .reduce(Max)
            .named("worst_node")
            .evaluate(&snap, 1, 1);

        assert_eq!(rows.len(), 2, "buckets survive the reduce");
        assert!(rows.iter().all(|r| r.group.is_none()));
        assert!(is_close(rows[0].value, 20.0));
        assert!(is_close(rows[1].value, 200.0));
    }

    #[test]
    fn reduce_without_buckets_pools_every_observation() {
        // No shared time grid, so a global percentile over the raw
        // observations of both nodes.
        let snap = snapshot(
            &[
                (r#"latency{instance="a"}"#, &[(1, 1.0), (2, 2.0)]),
                (r#"latency{instance="b"}"#, &[(3, 3.0), (4, 4.0)]),
            ],
            10,
        );
        let rows = MetricQuery::select("latency")
            .reduce(Percentile(0.5))
            .named("global_p50")
            .evaluate(&snap, 1, 1);

        assert_eq!(rows.len(), 1);
        assert!(is_close(rows[0].value, 2.5), "median of 1,2,3,4");
        assert_eq!(
            (rows[0].bucket_start_ms, rows[0].bucket_end_ms),
            (WHOLE_RUN_MS, WHOLE_RUN_MS),
            "a pooled reduce covers the run, not the span its points occupied"
        );
    }

    #[test]
    fn a_pooled_reduce_groups_across_seeds_rather_than_per_seed() {
        // Two seeds whose observations occupy different simulated spans. The
        // summary must still see one window with two runs in it.
        let plan = MetricQuery::select("latency")
            .reduce(Percentile(0.99))
            .named("global_p99");
        let rows: Vec<MetricQueryRow> = [(1u64, 5u64), (2, 900)]
            .iter()
            .flat_map(|(seed, last_ms)| {
                let snap = snapshot(&[("latency", &[(1, 1.0), (*last_ms, 2.0)])], 1_000);
                plan.evaluate(&snap, 7, *seed)
            })
            .collect();
        let report = MetricQueryReport::from_rows(&plan, rows);

        assert_eq!(report.windows.len(), 1, "one window, not one per seed");
        assert_eq!(report.windows[0].runs, 2);
    }

    #[test]
    fn reduce_by_groups_on_one_label() {
        let snap = snapshot(
            &[
                (r#"v{instance="a",zone="eu"}"#, &[(0, 1.0)]),
                (r#"v{instance="b",zone="eu"}"#, &[(0, 3.0)]),
                (r#"v{instance="c",zone="us"}"#, &[(0, 10.0)]),
                (r#"v{instance="d"}"#, &[(0, 99.0)]),
            ],
            60_000,
        );
        let rows = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .reduce_by("zone", Mean)
            .named("per_zone")
            .evaluate(&snap, 1, 1);

        assert_eq!(rows.len(), 2, "the unlabelled series is dropped");
        assert_eq!(rows[0].group.as_deref(), Some("eu"));
        assert!(is_close(rows[0].value, 2.0));
        assert_eq!(rows[1].group.as_deref(), Some("us"));
        assert!(is_close(rows[1].value, 10.0));
    }

    // --- metadata --------------------------------------------------------

    #[test]
    fn every_row_carries_its_run_id_and_seed() {
        let snap = snapshot(&[("v", &[(0, 1.0), (60_000, 2.0)])], 120_000);
        let rows = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .named("q")
            .evaluate(&snap, 0xDEAD_BEEF, 9182);

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.run_id == 0xDEAD_BEEF));
        assert!(rows.iter().all(|r| r.seed == 9182));
        assert!(rows.iter().all(|r| r.query == "q"));
    }

    #[test]
    fn provenance_records_the_last_collapse() {
        let raw = MetricQuery::select("v").rate().named("q");
        assert_eq!(raw.provenance(), Provenance::Observations);

        let scalar = MetricQuery::select("v")
            .bucketize(Duration::from_secs(1), Mean)
            .named("q");
        assert_eq!(scalar.provenance(), Provenance::Scalar);

        let quantile = MetricQuery::select("v")
            .bucketize(Duration::from_secs(1), Percentile(0.99))
            .named("q");
        assert_eq!(quantile.provenance(), Provenance::Quantile);

        // A percentile followed by an honest mean is still not a percentile.
        let mixed = MetricQuery::select("v")
            .bucketize(Duration::from_secs(1), Percentile(0.99))
            .reduce(Mean)
            .named("q");
        assert_eq!(mixed.provenance(), Provenance::Scalar);
    }

    #[test]
    fn description_renders_the_pipeline() {
        let plan = MetricQuery::select("requests_total")
            .label("operation", "write")
            .rate()
            .bucketize(Duration::from_mins(1), Mean)
            .reduce(Max)
            .named("write_throughput");
        assert_eq!(
            plan.description(),
            r#"requests_total{operation="write"} → rate → 60s buckets (mean) → reduce (max)"#
        );
    }

    // --- cross-run summary -----------------------------------------------

    fn multi_seed_rows(values: &[(u64, f64)]) -> (MetricQueryPlan, Vec<MetricQueryRow>) {
        let plan = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .reduce(Mean)
            .named("throughput");
        let rows = values
            .iter()
            .flat_map(|(seed, value)| {
                let snap = snapshot(&[("v", &[(0, *value)])], 60_000);
                plan.evaluate(&snap, 7, *seed)
            })
            .collect();
        (plan, rows)
    }

    #[test]
    fn multi_seed_summary_names_the_extreme_seeds() {
        let (plan, rows) =
            multi_seed_rows(&[(9182, 5_421.0), (224, 6_102.0), (3, 5_900.0), (4, 5_950.0)]);
        let report = MetricQueryReport::from_rows(&plan, rows);

        assert_eq!(report.runs, 4);
        assert_eq!(report.windows.len(), 1, "one window, so a flat summary");
        let w = &report.windows[0];
        assert!(is_close(w.min, 5_421.0));
        assert_eq!(w.min_seed, 9182, "replay this one");
        assert!(is_close(w.max, 6_102.0));
        assert_eq!(w.max_seed, 224);
        assert!(is_close(w.mean, 5_843.25));
        assert!(is_close(w.p50, 5_925.0));
    }

    #[test]
    fn extreme_seed_ties_break_on_the_smallest_seed() {
        let (plan, rows) = multi_seed_rows(&[(99, 1.0), (7, 1.0), (55, 1.0)]);
        let report = MetricQueryReport::from_rows(&plan, rows);
        assert_eq!(report.windows[0].min_seed, 7);
        assert_eq!(report.windows[0].max_seed, 7);
    }

    #[test]
    fn summary_windows_are_deterministically_ordered() {
        let plan = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .reduce_by("zone", Mean)
            .named("per_zone");

        let build = |seeds: &[u64]| {
            let rows: Vec<MetricQueryRow> = seeds
                .iter()
                .flat_map(|seed| {
                    let snap = snapshot(
                        &[
                            (r#"v{zone="us"}"#, &[(0, 2.0), (60_000, 4.0)]),
                            (r#"v{zone="eu"}"#, &[(0, 1.0), (60_000, 3.0)]),
                        ],
                        120_000,
                    );
                    plan.evaluate(&snap, 7, *seed)
                })
                .collect();
            MetricQueryReport::from_rows(&plan, rows)
                .windows
                .iter()
                .map(|w| (w.group.clone(), w.bucket_start_ms))
                .collect::<Vec<_>>()
        };

        let forward = build(&[1, 2, 3]);
        let backward = build(&[3, 2, 1]);
        assert_eq!(forward, backward, "seed order must not reorder the report");
        assert_eq!(
            forward,
            vec![
                (Some("eu".to_owned()), 0),
                (Some("eu".to_owned()), 60_000),
                (Some("us".to_owned()), 0),
                (Some("us".to_owned()), 60_000),
            ]
        );
    }

    #[test]
    fn a_bucketed_query_summarizes_each_bucket_separately() {
        let plan = MetricQuery::select("v")
            .bucketize(Duration::from_mins(1), Mean)
            .named("bucketed");
        let rows: Vec<MetricQueryRow> = [(1u64, 1.0), (2, 3.0)]
            .iter()
            .flat_map(|(seed, base)| {
                let snap = snapshot(&[("v", &[(0, *base), (60_000, base * 10.0)])], 120_000);
                plan.evaluate(&snap, 7, *seed)
            })
            .collect();
        let report = MetricQueryReport::from_rows(&plan, rows);

        assert_eq!(report.windows.len(), 2);
        assert_eq!(report.runs, 2);
        assert!(is_close(report.windows[0].mean, 2.0));
        assert!(is_close(report.windows[1].mean, 20.0));
        assert_eq!(report.windows[1].max_seed, 2);
    }

    #[test]
    fn percentile_helpers_handle_degenerate_input() {
        assert!(Aggregator::Mean.apply(&[]).is_none());
        assert!(is_close(percentile_of_sorted(&[4.0], 0.99), 4.0));
        assert!(is_close(percentile_of_sorted(&[1.0, 2.0], 0.0), 1.0));
        assert!(is_close(percentile_of_sorted(&[1.0, 2.0], 1.0), 2.0));
        assert!(is_close(percentile_of_sorted(&[1.0, 2.0], f64::NAN), 1.0));
        assert!(is_close(percentile_of_sorted(&[1.0, 2.0], 5.0), 2.0));
    }

    #[test]
    fn aggregator_labels_read_as_percentiles() {
        assert_eq!(Aggregator::Percentile(0.99).label(), "p99");
        assert_eq!(Aggregator::Percentile(0.5).label(), "p50");
        assert_eq!(Aggregator::Percentile(0.999).label(), "p99.9");
        assert_eq!(Aggregator::Mean.label(), "mean");
    }
}
