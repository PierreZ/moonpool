# Application Metrics

<!-- toc -->

Your service is already instrumented. Somewhere there is a `prometheus::Registry`, a `requests_total` counter, a latency histogram, a gauge for queue depth. That instrumentation exists because it is how you understand the system in production.

Moonpool reports on it. Register one metrics source per simulated node, and the counters, gauges and histograms your code already keeps appear in the simulation report: aggregated across seeds, attributed per node, with an exact time series of every change.

```rust,ignore
use std::sync::Arc;
use moonpool_prometheus::PrometheusSource;

let report = SimulationBuilder::new()
    .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
    .processes(3, || Box::new(MeteredKvNode::new()))
    .workload(MeteredKvWorkload::default())
    .set_iterations(10)
    .run()?;

report.eprint();
```

```text
━━━ Metrics (15 series) ━━━━━━━━━━━━━━━━━━━━━━━━━━
  kv_keys_stored{instance="10.0.1.1"}                  15.81 avg        1 min       16 max
  kv_request_latency_seconds{instance="10.0.1.1"}     130.83 total  6,667 obs     0.02 avg
  kv_requests_in_flight{instance="10.0.1.1"}            2.22 avg         0 min        3 max
  kv_requests_rejected_total{instance="10.0.1.1"}      8,620 total     862 per seed
  kv_requests_served_total{instance="10.0.1.1"}        6,667 total  666.70 per seed
```

The runnable version is [`crates/moonpool-sim-examples/src/metrics_service.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim-examples/src/metrics_service.rs): a key/value node with admission control, metered the way a production service is.

## Reading Metrics from Process Code

The source is reachable from any process or workload through its context:

```rust,ignore
let source = ctx.metrics::<PrometheusSource>()
    .ok_or_else(|| invalid_state("no metrics factory registered"))?;

let served = source.counter("kv_requests_served_total", "Requests served")?;
let in_flight = source.gauge("kv_requests_in_flight", "Requests currently being served")?;
let latency = source.histogram_with_buckets(
    "kv_request_latency_seconds",
    "Time to serve one request",
    vec![0.001, 0.005, 0.010, 0.025, 0.050, 0.100, 0.250],
)?;

served.inc();
```

`counter`, `gauge`, `histogram`, `histogram_with_buckets` and `histogram_with_opts` are get-or-register. The vector forms `counter_vec`, `gauge_vec` and `histogram_vec` take label names and hand out children with `with_label_values`. Get-or-register matters because a source outlives process reboots. A rebooted process looks its metrics up instead of failing with `AlreadyReg`. The lookup checks the shape. A second registration of a name with other label names, or a histogram with other buckets, returns an error, because the cached metric would record the values under the wrong labels or in the wrong buckets. A different help text is accepted, and the first one stays.

To wrap a registry your application already built, use `PrometheusSource::new(registry)`.

## Why Per Node, and Why Per Iteration

Every simulated node shares one OS process. One shared registry would merge three nodes' counters into one number. So the factory is keyed by IP, and each sample carries an `instance` label with the node's IP, Prometheus's own label for a scrape target.

The factory also runs again every iteration. Most registries have no reset, so a reused instance would make seed 50 report the sum of fifty runs. A [custom fault injector](./31-custom-faults.md) uses `fault_factory` for the same reason.

A **reboot** is the exception. The source is keyed by IP, so a restarted process keeps its counters, as a real node's `/metrics` does across a restart on the same host.

## Time Series, Not Samples

Metrics created through the source return instrumented handles: `SimCounter`, `SimGauge`, `SimHistogram`. Every `inc()`, `set()` and `observe()` records a point stamped with the **simulated** clock. There is no scrape interval. The series is exact, and it replays bit for bit for a given seed.

```rust,ignore
let metrics = report.individual_metrics[0].as_ref().expect("seed passed");

for (series, points) in &metrics.app_series {
    for point in points {
        println!("{series} {} at {}ms", point.value, point.time_ms);
    }
}
```

That is what lines a latency spike up with the partition that caused it. The aggregate on `report.app_metrics` (`MetricAggregate`) uses the series for `min`, `max` and `mean`, so a gauge reports the range it moved over, not the value it rested at when the run ended.

Series are capped at 10,000 points each (`DEFAULT_SERIES_CAPACITY`, tunable with `PrometheusSource::set_series_capacity`), so a hot loop cannot grow a test's memory without bound. `SimulationMetrics::dropped_metric_points` is non-zero when a series was truncated.

## Timing on the Simulated Clock

Use the handle's `start_timer`, not `prometheus`'s own:

```rust,ignore
let timer = latency.start_timer(ctx.time());
let response = client.request(req).await?;
timer.stop_and_record();
```

`prometheus::Histogram::start_timer()` reads the wall clock. Inside a simulation that records how fast your laptop was, and it differs on every replay of the same seed. `SimHistogram::start_timer` takes any `TimeProvider` and reads the simulated clock.

Pick bucket bounds that bracket what the simulation produces. Prometheus's defaults are tuned for real HTTP latency. A distribution that lands in one bucket tells you nothing.

## Reading Metrics in check()

A workload can read every node's source through `ctx.metrics_handle()`. That is how a test asserts on what the system recorded about itself:

```rust,ignore
async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    let handle = ctx.metrics_handle();
    let samples = handle.collect_all();
    assert_always!(!samples.is_empty(), "the nodes recorded something over five simulated seconds");

    let series = handle.collect_series();
    assert_sometimes!(
        series.keys().any(|k| k.starts_with("kv_request_latency")),
        "latency observations are recorded as a time series"
    );
    Ok(())
}
```

## Turning Series into Answers

After five hundred seeds the question is "what throughput did this hold, and which seed was worst?" The report already answers the first part: `report.app_metrics` folds every series across the successful seeds. For anything finer, the series are plain data. `individual_metrics` keeps each seed's `app_series`, keyed by the series identity (`SeriesKey` parses it back into a name and labels), so a test or a tool computes the rate, bucket or percentile it needs in ordinary Rust, and names the seed it came from.

## Worked Example: a Metastable Failure

Totals say what a run did. The series says **when** it went wrong, which is the only way to see a failure that outlives its cause. [`crates/moonpool/examples/metastable_grpc_retry_storm.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool/examples/metastable_grpc_retry_storm.rs) is the demonstration: a real gRPC service with a bounded worker pool, an open-loop client with an aggressive timeout and retries, and one temporary slowdown in the middle of the run.

```bash
cargo run --release --example metastable_grpc_retry_storm \
    --features hyper,prometheus -- --seed 0
```

It buckets the recorded series (a rate for each counter, a mean for each gauge) and prints them as an ASCII graph, one column per simulated second. The slowdown lasts 650 ms. The service never recovers: retries from that window keep the server saturated for the remaining 29 seconds at the same offered load it carried before. The trigger is gone, the bad state is not.

`--search 0..64` runs a range of seeds and prints one line each. Some seeds absorb the trigger and others fall into the storm. Same code, same configuration, different seed. That is the evidence that the bad state is a second stable operating point.

## What Metrics Are Not

Metric values are **reported, never used to steer the simulation**. Not everything in a registry is deterministic. A metric fed from the wall clock is not. The runner treats a scrape as an observation. To enforce a property, [assert](./12-assertions.md) on it: metrics tell you the shape of a run, assertions tell you whether it was correct.

Under [fork-based exploration](../part5-building-on-top/01-exploration.md), only root timelines are scraped. Explored timelines report through the shared assertion table, which carries no metric values.

## Beyond Prometheus

`PrometheusSource` is one implementation of `MetricsSource`, a registry-agnostic trait in `moonpool-core`:

```rust,ignore
pub trait MetricsSource: Send + Sync + 'static {
    fn collect(&self) -> Vec<MetricSample>;
    fn set_clock(&self, clock: Arc<dyn MetricClock>) {}
    fn series(&self) -> BTreeMap<String, Vec<MetricPoint>> { BTreeMap::new() }
    fn dropped_points(&self) -> u64 { 0 }
}
```

`collect` alone makes a scrape-only adapter over any registry. A scrape-only series becomes one point at the end of the run. A scrape-only **histogram** becomes nothing: its scrape holds bucket counts and a sum, not the observations, and a p99 of the sum is a wrong number. Record a histogram through an instrumented handle to query it. The other three deliver an exact time series, with `SeriesRecorder` from `moonpool-core` doing the stamping and capping. An OpenTelemetry adapter slots in the same way, with no change to the runner.
