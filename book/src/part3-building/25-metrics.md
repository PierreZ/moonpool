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

`counter`, `gauge`, `histogram`, `histogram_with_buckets` and `histogram_with_opts` are get-or-register. The vector forms `counter_vec`, `gauge_vec` and `histogram_vec` take label names and hand out children with `with_label_values`. Get-or-register matters because a source outlives process reboots. A rebooted process looks its metrics up instead of failing with `AlreadyReg`.

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

`prometheus::Histogram::start_timer()` reads the wall clock. Inside a simulation that records how fast your laptop was, and it differs on every replay of the same seed. `SimHistogram::start_timer` takes any `TimeProvider` and reads the simulated clock. `stop_and_discard()` drops a timing you do not want, and `observe_duration(d)` records a `Duration` you measured yourself.

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

## Asking Questions: Metric Queries

A raw series is data, not an answer. After five hundred seeds the question is "what throughput did this hold, and which seed was worst?" So the **runner declares what it wants to know**, and the report answers:

```rust,ignore
use std::time::Duration;
use moonpool_sim::{Fill, Mean, MetricQuery, Percentile};

let report = SimulationBuilder::new()
    .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
    .metric(
        MetricQuery::select("requests_total")
            .label("operation", "write")
            .rate()
            .bucketize(Duration::from_secs(60), Mean)
            // A minute with no requests is zero per second, not missing data.
            .fill(Fill::Value(0.0))
            .reduce(Mean)
            .named("write_throughput"),
    )
    .metric(
        MetricQuery::select("request_latency_seconds")
            .bucketize(Duration::from_secs(60), Percentile(0.99))
            .named("write_p99"),
    )
    .processes(3, || Box::new(MyNode::new()))
    .workload(MyWorkload::default())
    .set_iterations(500)
    .run()?;
```

```text
━━━ Metric Queries (2) ━━━━━━━━━━━━━━━━━━━━━━━━━━
  write_throughput
    requests_total{operation="write"} → rate → 60s buckets (mean) → reduce (mean)  ·  scalar per run  ·  runs: 500
    min            5,421   seed=9182
    mean           5,973
    max            6,102   seed=224
    across runs:  p50 5,991  p95 6,071  p99 6,094
```

`min` and `max` name the seed, so the worst run goes straight into `set_debug_seeds(vec![9182])`. Results travel with the report in `SimulationReport::metric_queries`, one `MetricQueryReport` per query, with every per-seed `MetricQueryRow`.

### The Operations

| Op | What it does |
|----|--------------|
| `select(name)` + `.label(k, v)` | pick series by name and label subset |
| `.rate()` | monotonic counter to per-second rate, on simulated time |
| `.bucketize(width, agg)` | regularize into fixed simulated-time buckets |
| `.fill(policy)` | repeat an existing value into empty buckets |
| `.interpolate()` | compute empty buckets from their neighbours |
| `.map(window, agg)` | rolling window over one series |
| `.reduce(agg)` / `.reduce_by(label, agg)` | combine across series |
| `.named(name)` | finish the plan |

The aggregators are `Min`, `Mean`, `Max` and `Percentile(p)`. Nothing else.

**Counter semantics are explicit.** `.rate()` is the only place a series is read as a counter. `bucketize(_, Mean)` on a raw counter reports the mean cumulative value, not a throughput. A drop in a counter is read as a reset, Prometheus style. Every counter starts at zero at simulated time zero, so the first increment is rated too.

### Empty Buckets

`bucketize` omits a bucket nothing landed in, because a gap and a zero are different facts. Two ops say which one you mean, the way Warp 10 splits them. `.fill` **repeats a value the series has**. `.interpolate` **computes one it never had**.

| Op | Gives empty buckets | Leaves empty |
|----|---------------------|--------------|
| `.fill(Fill::Previous)` | the last known value | leading gaps |
| `.fill(Fill::Next)` | the next known value | trailing gaps |
| `.fill(Fill::Value(v))` | `v`, no neighbour needed | nothing |
| `.interpolate()` | a point on the line between neighbours | leading and trailing gaps |

```text
bucket                     0 |    1 |    2 |    3 |    4
no fill                   10 |    - |    - |   40 |    -
.fill(Fill::Previous)     10 |   10 |   10 |   40 |   40
.fill(Fill::Next)         10 |   40 |   40 |   40 |    -
.fill(Fill::Value(0.0))   10 |    0 |    0 |   40 |    0
.interpolate()            10 |   20 |   30 |   40 |    -
```

Use a fill when the quantity holds between observations, or is zero when nothing happened. Use `.interpolate()` when it moves continuously: a queue depth, a replication lag. They compose in call order. The grid runs from bucket zero to the run's end, so with `Fill::Value(0.0)` every window sees every seed and `min` and `max` compare like with like.

**Series identity is name plus label set.** Matching is by subset: a query naming `operation="write"` also matches a series carrying `instance="10.0.1.1"`. Every result row carries `seed` and `run_id` as moonpool fields, never injected into your labels.

### Percentiles That Mean Something

`mean(p99(node_a), p99(node_b))` is not a p99, and neither is `p99(p99(a), p99(b))`. Once observations collapse into a percentile, a percentile of that has no meaning.

The builder enforces this at compile time. Each stage tracks what its values are, and `Percentile` applies only where the observations survive:

```rust,ignore
// Compiles: the raw histogram observations are still there.
MetricQuery::select("request_latency_seconds")
    .bucketize(Duration::from_secs(60), Percentile(0.99))
    .named("p99");

// Does NOT compile: p99 of per-bucket p99s.
MetricQuery::select("request_latency_seconds")
    .bucketize(Duration::from_secs(60), Percentile(0.99))
    .reduce(Percentile(0.99))
    .named("nonsense");
```

`Min`, `Mean` and `Max` apply anywhere. "The mean of each node's p99" is a useful number as long as nobody calls it a p99, which is why the report prints each query's provenance: `observations`, `scalar` or `percentile-derived`. The `p50`, `p95` and `p99` in the summary block are a different dimension, taken **across runs** with one value per seed.

## Worked Example: a Metastable Failure

Totals say what a run did. The series says **when** it went wrong, which is the only way to see a failure that outlives its cause. [`crates/moonpool/examples/metastable_grpc_retry_storm.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool/examples/metastable_grpc_retry_storm.rs) is the demonstration: a real gRPC service with a bounded worker pool, an open-loop client with an aggressive timeout and retries, and one temporary slowdown in the middle of the run.

```bash
cargo run --release --example metastable_grpc_retry_storm \
    --features hyper,prometheus -- --seed 1
```

It runs query pipelines over the recorded series and prints the buckets as an ASCII graph, one column per simulated second. The slowdown lasts 650 ms. The service never recovers: retries from that window keep the server saturated for the remaining 29 seconds at the same offered load it carried before. The trigger is gone, the bad state is not.

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

`collect` alone makes a scrape-only adapter over any registry. The other three deliver an exact time series, with `SeriesRecorder` from `moonpool-core` doing the stamping and capping. An OpenTelemetry adapter slots in the same way, with no change to the runner.
