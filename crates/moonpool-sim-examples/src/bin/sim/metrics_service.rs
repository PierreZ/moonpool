//! Binary target for the metered key/value simulation.
//!
//! Three nodes, each with its own `prometheus::Registry`, running under
//! network chaos. The report's Metrics section shows what they recorded, and
//! the Metric Queries section answers the two questions this simulation is
//! actually about: what throughput did it sustain, and how bad did the tail
//! latency get.

use std::process;
use std::sync::Arc;
use std::time::Duration;

use moonpool_prometheus::PrometheusSource;
use moonpool_sim::{Chaos, ChaosMode, Mean, MetricQuery, Percentile, SimulationBuilder};
use moonpool_sim_examples::metrics_service::{MeteredKvNode, MeteredKvWorkload};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = SimulationBuilder::new()
        // One registry per node: every simulated node shares this OS process,
        // so a single shared registry would merge their counters into one.
        .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
        .processes(3, || Box::new(MeteredKvNode::new()))
        .workload(MeteredKvWorkload)
        // The runner declares what is worth summarizing across seeds. Both
        // read the series the instrumented handles already record.
        .metric(
            MetricQuery::select("kv_requests_served_total")
                // Counter semantics are explicit: without .rate() this would
                // average the cumulative total, not the throughput.
                .rate()
                .bucketize(Duration::from_secs(1), Mean)
                .reduce(Mean)
                .named("served_throughput"),
        )
        .metric(
            // Valid because the histogram handle records every observation, so
            // the underlying distribution is still there to take a p99 of.
            MetricQuery::select("kv_request_latency_seconds")
                .reduce(Percentile(0.99))
                .named("latency_p99"),
        )
        .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
        .chaos_duration(Duration::from_secs(3))
        .set_iterations(10)
        .run();

    report.eprint();

    if !report.seeds_failing.is_empty() {
        eprintln!(
            "ERROR: {} seeds failed: {:?}",
            report.seeds_failing.len(),
            report.seeds_failing
        );
        process::exit(1);
    }
}
