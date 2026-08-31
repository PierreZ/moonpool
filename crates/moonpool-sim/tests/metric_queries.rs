//! End-to-end: a runner declares metric queries, and their per-seed results
//! reach the report with the seed and run id that produced them.
//!
//! The metrics source here is hand-rolled rather than a `prometheus::Registry`
//! so the test exercises the runner wiring, not an adapter: what matters is
//! that `SeriesRecorder` points recorded on the simulated clock flow into
//! `SimulationReport::metric_queries`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Max, Mean, MetricPoint, MetricQuery, MetricSample, MetricValue, MetricsSource, Percentile,
    Provenance, SeriesRecorder, SimContext, SimulationBuilder, SimulationReport, SimulationResult,
    TimeProvider, Workload,
};

/// A minimal [`MetricsSource`]: one counter and one latency series, recorded
/// through a [`SeriesRecorder`] so both carry simulated timestamps.
struct Counters {
    recorder: SeriesRecorder,
    total: std::sync::atomic::AtomicU64,
}

impl Counters {
    fn new() -> Self {
        Self {
            recorder: SeriesRecorder::new(),
            total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Increment `requests_total{operation="write"}`, recording the new
    /// cumulative value the way an instrumented counter handle would.
    fn request(&self) {
        let next = self
            .total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let value = u32::try_from(next).map_or(f64::INFINITY, f64::from);
        self.recorder
            .record(r#"requests_total{operation="write"}"#, value);
    }

    /// Observe one latency, as a histogram handle would: the observation
    /// itself, not a running total.
    fn latency(&self, seconds: f64) {
        self.recorder.record("request_latency_seconds", seconds);
    }
}

impl MetricsSource for Counters {
    fn collect(&self) -> Vec<MetricSample> {
        let total = self.total.load(std::sync::atomic::Ordering::Relaxed);
        vec![MetricSample::new(
            "requests_total",
            vec![("operation".to_owned(), "write".to_owned())],
            MetricValue::Counter(u32::try_from(total).map_or(f64::INFINITY, f64::from)),
        )]
    }

    fn set_clock(&self, clock: Arc<dyn moonpool_sim::MetricClock>) {
        self.recorder.set_clock(clock);
    }

    fn series(&self) -> BTreeMap<String, Vec<MetricPoint>> {
        self.recorder.series()
    }
}

/// Issues `requests` requests, one every 100 simulated milliseconds, with a
/// latency that grows with the request index so percentiles have a shape.
struct MeteredWorkload {
    requests: u64,
}

#[async_trait]
impl Workload for MeteredWorkload {
    fn name(&self) -> &'static str {
        "metered"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let metrics = ctx.metrics::<Counters>().expect("metrics factory set");
        for i in 0..self.requests {
            ctx.time().sleep(Duration::from_millis(100)).await.ok();
            metrics.request();
            let index = u32::try_from(i).map_or(f64::INFINITY, f64::from);
            metrics.latency(0.001 * (index + 1.0));
        }
        Ok(())
    }
}

/// Run `seeds` iterations with two queries registered: a bucketized throughput
/// and a bucketized p99 latency.
fn run_with_queries(seeds: &[u64], requests: u64) -> SimulationReport {
    SimulationBuilder::new()
        .set_debug_seeds(seeds.to_vec())
        .set_iterations(seeds.len())
        .metrics_factory(|_ip| Arc::new(Counters::new()))
        .metric(
            MetricQuery::select("requests_total")
                .label("operation", "write")
                .rate()
                .bucketize(Duration::from_mins(1), Mean)
                .reduce(Mean)
                .named("write_throughput"),
        )
        .metric(
            MetricQuery::select("request_latency_seconds")
                .bucketize(Duration::from_mins(1), Percentile(0.99))
                .named("write_p99"),
        )
        .workload(MeteredWorkload { requests })
        .run()
}

fn query<'a>(report: &'a SimulationReport, name: &str) -> &'a moonpool_sim::MetricQueryReport {
    report
        .metric_queries
        .iter()
        .find(|q| q.name == name)
        .unwrap_or_else(|| panic!("{name} missing from {:?}", report.metric_queries))
}

#[test]
fn declared_queries_reach_the_report_with_seed_and_run_id() {
    let seeds = vec![11, 22, 33];
    let report = run_with_queries(&seeds, 20);
    assert_eq!(report.failed_runs, 0, "workload should succeed");
    assert_eq!(
        report.metric_queries.len(),
        2,
        "one report per registered query, in registration order"
    );
    assert_eq!(report.metric_queries[0].name, "write_throughput");
    assert_eq!(report.metric_queries[1].name, "write_p99");

    let throughput = query(&report, "write_throughput");
    assert_eq!(throughput.runs, seeds.len());
    assert!(!throughput.rows.is_empty(), "queries produced rows");

    // run_id identifies the invocation; the seed identifies the replay.
    assert_ne!(report.run_id, 0, "a run id was assigned");
    assert!(
        throughput.rows.iter().all(|r| r.run_id == report.run_id),
        "every row carries the report's run id"
    );
    let seeds_seen: std::collections::BTreeSet<u64> =
        throughput.rows.iter().map(|r| r.seed).collect();
    assert_eq!(
        seeds_seen,
        seeds.iter().copied().collect(),
        "every seed contributed, and is identifiable"
    );
}

#[test]
fn rate_reads_the_workloads_real_throughput() {
    // 20 requests, one per 100ms: 10 per simulated second, and the counter
    // starts at zero, so the whole run averages 10/s.
    let report = run_with_queries(&[7], 20);
    let throughput = query(&report, "write_throughput");
    assert_eq!(throughput.windows.len(), 1, "one 60s bucket");
    let window = &throughput.windows[0];
    assert!(
        (window.mean - 10.0).abs() < 0.01,
        "expected ~10 req/s, got {}",
        window.mean
    );
    assert_eq!(window.min_seed, 7);
    assert_eq!(window.max_seed, 7);
}

#[test]
fn provenance_distinguishes_a_percentile_from_a_mean() {
    let report = run_with_queries(&[1], 10);
    assert_eq!(
        query(&report, "write_throughput").provenance,
        Provenance::Scalar,
        "mean-then-mean is not a percentile"
    );
    assert_eq!(
        query(&report, "write_p99").provenance,
        Provenance::Quantile,
        "collapsed by a percentile over the raw observations"
    );
}

#[test]
fn the_worst_seed_is_named_and_replayable() {
    // Different request counts per seed would need different workloads, so
    // instead vary the seed set and check the extremes are attributed to a
    // real seed that can be fed back into set_debug_seeds.
    let report = run_with_queries(&[101, 202, 303], 15);
    let latency = query(&report, "write_p99");
    let window = &latency.windows[0];
    assert!(
        [101, 202, 303].contains(&window.max_seed),
        "max is attributed to a seed that ran"
    );

    // Replaying that seed alone reproduces the same value.
    let replay = run_with_queries(&[window.max_seed], 15);
    let replayed = &query(&replay, "write_p99").windows[0];
    assert!(
        (replayed.max - window.max).abs() < 1e-9,
        "replaying the named seed reproduces the value: {} vs {}",
        replayed.max,
        window.max
    );
}

#[test]
fn report_ordering_does_not_depend_on_seed_order() {
    let forward = run_with_queries(&[5, 6, 7], 12);
    let backward = run_with_queries(&[7, 6, 5], 12);

    let shape = |report: &SimulationReport| {
        report
            .metric_queries
            .iter()
            .map(|q| {
                (
                    q.name.clone(),
                    q.windows
                        .iter()
                        .map(|w| (w.group.clone(), w.bucket_start_ms, w.bucket_end_ms))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&forward), shape(&backward));

    // And the printed report is byte-identical for the same seed set.
    let repeat = run_with_queries(&[5, 6, 7], 12);
    let rendered = |report: &SimulationReport| {
        report
            .metric_queries
            .iter()
            .map(|q| format!("{}|{:?}", q.name, summary_of(q)))
            .collect::<Vec<_>>()
    };
    assert_eq!(rendered(&forward), rendered(&repeat));
}

/// The numbers a report prints for one query, as a comparable tuple list.
fn summary_of(query: &moonpool_sim::MetricQueryReport) -> Vec<(u64, u64, u64, u64)> {
    query
        .windows
        .iter()
        .map(|w| (w.bucket_start_ms, w.bucket_end_ms, w.min_seed, w.max_seed))
        .collect()
}

#[test]
fn a_query_matching_nothing_reports_no_runs() {
    let report = SimulationBuilder::new()
        .set_debug_seeds(vec![1])
        .set_iterations(1)
        .metrics_factory(|_ip| Arc::new(Counters::new()))
        .metric(
            MetricQuery::select("no_such_metric")
                .reduce(Max)
                .named("nope"),
        )
        .workload(MeteredWorkload { requests: 3 })
        .run();

    let nope = query(&report, "nope");
    assert_eq!(nope.runs, 0);
    assert!(nope.rows.is_empty());
    assert!(nope.windows.is_empty());
}
