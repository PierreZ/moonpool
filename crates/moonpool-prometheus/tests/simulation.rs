//! End-to-end: a workload's own prometheus metrics land in the simulation report.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_prometheus::PrometheusSource;
use moonpool_sim::{SimContext, SimulationBuilder, SimulationResult, TimeProvider, Workload};

/// Drives a fixed number of "requests", recording each one the way production
/// code would: a counter, a gauge, and a latency histogram.
struct MeteredWorkload {
    requests: u64,
}

#[async_trait]
impl Workload for MeteredWorkload {
    fn name(&self) -> &'static str {
        "metered"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let metrics = ctx
            .metrics::<PrometheusSource>()
            .expect("metrics factory registered");
        let processed = metrics
            .counter("requests_total", "Requests processed")
            .expect("counter registers");
        let in_flight = metrics
            .gauge("requests_in_flight", "Requests currently running")
            .expect("gauge registers");
        let latency = metrics
            .histogram_with_buckets(
                "request_latency_seconds",
                "Request latency",
                vec![0.005, 0.01, 0.05, 0.1],
            )
            .expect("histogram registers");

        for _ in 0..self.requests {
            in_flight.inc();
            // Timed on the simulated clock, so the observation is the latency
            // the simulation modeled and replays identically.
            let timer = latency.start_timer(ctx.time());
            ctx.time().sleep(Duration::from_millis(10)).await.ok();
            timer.stop_and_record();
            in_flight.dec();
            processed.inc();
        }
        Ok(())
    }
}

fn run_metered(requests: u64, seed: u64) -> moonpool_sim::SimulationReport {
    SimulationBuilder::new()
        .set_debug_seeds(vec![seed])
        .set_iterations(1)
        .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
        .workload(MeteredWorkload { requests })
        .run()
}

#[test]
fn counters_gauges_and_histograms_reach_the_report() {
    let report = run_metered(5, 42);
    assert_eq!(report.failed_runs, 0, "workload should succeed");

    let by_name = |name: &str| {
        report
            .app_metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("{name} missing from report: {:?}", report.app_metrics))
    };

    let requests = by_name("requests_total");
    assert_eq!(requests.kind, "counter");
    assert!(
        (requests.total - 5.0).abs() < f64::EPSILON,
        "one per request"
    );
    assert!(
        requests.key.contains("instance="),
        "series is attributed to its node: {}",
        requests.key
    );

    let in_flight = by_name("requests_in_flight");
    assert_eq!(in_flight.kind, "gauge");
    // Range comes from the recorded series, not the end-of-run scrape: the
    // workload runs one request at a time, so the gauge swings 0..1. A
    // scrape-only aggregate would report 0/0 and hide that entirely.
    assert!(
        (in_flight.min - 0.0).abs() < f64::EPSILON,
        "gauge returns to zero between requests, got {}",
        in_flight.min
    );
    assert!(
        (in_flight.max - 1.0).abs() < f64::EPSILON,
        "gauge peaks at one in-flight request, got {}",
        in_flight.max
    );
    assert_eq!(in_flight.observations, 10, "one point per inc() and dec()");

    let latency = by_name("request_latency_seconds");
    assert_eq!(latency.kind, "histogram");
    let histogram = latency.histogram.as_ref().expect("histogram buckets");
    assert_eq!(histogram.count, 5, "one observation per request");
    assert!(
        (histogram.mean() - 0.010).abs() < 1e-9,
        "each request slept 10ms of simulated time, got {}",
        histogram.mean()
    );
}

#[test]
fn recorded_series_tracks_every_mutation_in_simulated_time() {
    let report = run_metered(3, 7);
    let metrics = report.individual_metrics[0]
        .as_ref()
        .expect("iteration succeeded");

    let counter = metrics
        .app_series
        .iter()
        .find(|(key, _)| key.starts_with("requests_total"))
        .map(|(_, points)| points)
        .expect("counter series recorded");

    assert_eq!(counter.len(), 3, "one point per inc(), not a sampled total");
    assert_eq!(
        counter.iter().map(|p| p.value).collect::<Vec<_>>(),
        vec![1.0, 2.0, 3.0],
        "series carries the running value"
    );
    // 10ms of simulated sleep between each increment.
    assert_eq!(counter[0].time_ms, 10);
    assert_eq!(counter[1].time_ms, 20);
    assert_eq!(counter[2].time_ms, 30);
    assert_eq!(metrics.dropped_metric_points, 0, "nothing truncated");
}

#[test]
fn series_is_identical_across_replays_of_a_seed() {
    let first = run_metered(4, 1234);
    let second = run_metered(4, 1234);

    let series = |r: &moonpool_sim::SimulationReport| {
        r.individual_metrics[0]
            .as_ref()
            .expect("iteration succeeded")
            .app_series
            .clone()
    };

    let (a, b) = (series(&first), series(&second));
    assert!(!a.is_empty(), "something was recorded");
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "same series"
    );
    for (key, points) in &a {
        assert_eq!(
            points, &b[key],
            "series {key} must replay bit-for-bit from the same seed"
        );
    }
}
