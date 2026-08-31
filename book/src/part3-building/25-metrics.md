# Application Metrics

Your service is probably already instrumented. There is a `prometheus::Registry`
somewhere, a `requests_total` counter, a latency histogram, a gauge for the
queue depth. That instrumentation exists because it is how you understand the
system in production.

Moonpool can report on it. Register a metrics source per simulated node and the
counters, gauges and histograms your code already keeps show up in the
simulation report — aggregated across seeds, attributed per node, with an exact
time series of every mutation.

```rust,ignore
use std::sync::Arc;
use moonpool_prometheus::PrometheusSource;

let report = SimulationBuilder::new()
    .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
    .processes(3, || Box::new(MyNode::new()))
    .workload(MyWorkload::default())
    .set_iterations(10)
    .run();

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

## Reading your metrics from process code

The source is reachable from any process or workload through its context:

```rust,ignore
let metrics = ctx.metrics::<PrometheusSource>()
    .ok_or_else(|| SimulationError::InvalidState("no metrics factory".into()))?;

let served = metrics.counter("requests_served_total", "Requests served")?;
let in_flight = metrics.gauge("requests_in_flight", "Requests running")?;
let latency = metrics.histogram_with_buckets(
    "request_latency_seconds",
    "Time to serve one request",
    vec![0.001, 0.005, 0.010, 0.050, 0.100],
)?;

served.inc();
```

These are get-or-register, which matters because a source outlives process
reboots (see below). A rebooted process looks its metrics up instead of failing
with `AlreadyReg`.

To wrap a registry your application already built, use
`PrometheusSource::new(my_registry)`.

## Why per node, and why per iteration

Every simulated node shares one OS process. A single shared registry would
merge three nodes' counters into one number, so the factory is keyed by IP and
each node gets its own registry. Each sample is stamped with an `instance`
label naming the node — Prometheus' own label for a scrape target.

The factory also runs afresh every iteration. Most registries have no reset, so
a reused instance would make seed 50 report the sum of fifty runs. This is the
same reasoning behind [`fault_factory`](./07-chaos.md) versus `fault`.

A **reboot** is the exception: a source is keyed by IP, so a crashed and
restarted process keeps its counters, exactly as a real node's `/metrics` does
across a restart on the same host.

## Time series, not samples

Metrics created through the source hand back instrumented handles. Every
`inc()`, `set()` and `observe()` records a point, stamped with the **simulated**
clock. There is no scrape interval and no sampling: the series is exact, and it
replays bit-for-bit for a given seed.

```rust,ignore
let metrics = report.individual_metrics[0].as_ref().expect("seed passed");

for (series, points) in &metrics.app_series {
    for point in points {
        println!("{series} {} at {}ms", point.value, point.time_ms);
    }
}
```

That is what makes a latency spike line up with the partition that caused it.
The aggregated view on `report.app_metrics` uses this series for `min`, `max`
and `mean`, which is why a gauge reports the range it actually moved over
rather than whatever value it happened to rest at when the run ended.

Series are capped (10,000 points each by default; see
`PrometheusSource::set_series_capacity`) so a hot loop cannot grow a test's
memory without bound. `SimulationMetrics::dropped_metric_points` is non-zero
when a series was truncated.

## Asking questions: metric queries

A raw series is data, not an answer. After five hundred seeds the question is
usually "what throughput did this hold, and which seed was the worst?" — so the
**runner declares what it wants to know**, and the report answers it.

```rust,ignore
use std::time::Duration;
use moonpool_sim::{Mean, MetricQuery, Percentile};

let report = SimulationBuilder::new()
    .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
    .metric(
        MetricQuery::select("requests_total")
            .label("operation", "write")
            .rate()
            .bucketize(Duration::from_secs(60), Mean)
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
    .run();
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

`min` and `max` name the seed that produced them, so the worst run goes
straight into `set_debug_seeds(vec![9182])`.

Queries are declared before `run()` and their results travel with the report,
in `SimulationReport::metric_queries`. Nothing reconstructs them afterwards
from raw snapshots: the runner already knew what mattered.

### The five operations

| Op | What it does |
|----|--------------|
| `select(name)` + `.label(k, v)` | pick series by name and label subset |
| `.rate()` | monotonic counter → per-second rate, on simulated time |
| `.bucketize(width, agg)` | regularize into fixed simulated-time buckets |
| `.map(window, agg)` | rolling window over one series' points |
| `.reduce(agg)` / `.reduce_by(label, agg)` | combine across series |

The aggregators are `Min`, `Mean`, `Max` and `Percentile(p)` — nothing else.

**Counter semantics are explicit.** `.rate()` is the only place a series is
read as a counter; `bucketize(_, Mean)` on a raw counter honestly reports the
mean *cumulative value*, not a throughput. A drop in a counter's value is read
as a reset, Prometheus style. Because a fresh source is built per iteration,
every counter is zero at simulated time zero, so the very first increment is
rated too.

**Series identity is name plus label set**, canonically ordered, so two
label sets that differ only in insertion order are the same series. Matching is
by subset: a query naming `operation="write"` also matches a series carrying
`instance="10.0.1.1"`.

**Moonpool's own metadata stays out of your labels.** Every result row carries
`seed` (the replayable identity of one iteration) and `run_id` (the identity of
the whole `run()` invocation) as moonpool-side fields, never injected into the
application's Prometheus labels.

### Percentiles that mean something

`mean(p99(node_a), p99(node_b))` is not a p99, and `p99(p99(a), p99(b))` is not
one either. Once observations are collapsed into a percentile, the percentile
of *that* is a number without a meaning.

The builder enforces this at compile time. Each stage tracks what its values
are, and `Percentile` only applies where the individual observations survive:

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

`Min`, `Mean` and `Max` apply anywhere — "the mean of each node's p99" is a
useful number as long as nobody calls it a p99, which is why the report prints
each query's provenance (`observations`, `scalar` or `percentile-derived`).

The percentiles in the summary block are a different dimension: they are taken
*across runs*, where every seed contributes one equally-weighted value. `p95`
there names the 95th-percentile run, which is why the report labels it
`across runs`.

## Timing on the simulated clock

Use `start_timer` from this crate, not `prometheus`' own:

```rust,ignore
let timer = latency.start_timer(ctx.time());
let response = client.request(req).await?;   // observed even on an early return
timer.stop_and_record();
```

`prometheus::Histogram::start_timer()` reads the wall clock. Inside a
simulation that records how fast your laptop happened to be, not the latency
the simulation modeled, and it gives a different answer on every replay of the
same seed. `SimHistogram::start_timer` reads the provider clock instead.

Pick bucket bounds that bracket what your simulation produces.
Prometheus' defaults are tuned for real HTTP latency; simulated latencies often
live on a different scale, and a distribution that lands entirely in the last
bucket tells you nothing.

## What metrics are not

Metric values are **reported, never used to steer the simulation**. Not
everything in a registry is deterministic — a metric fed from the wall clock,
or from a library's own internal timing, is not — so the runner treats a scrape
as an observation. If you want a property enforced, assert on it: metrics tell
you the shape of a run, [assertions](./12-assertions.md) tell you whether it was
correct.

Under [fork-based exploration](../part5-building-on-top/01-exploration.md), only
root timelines are scraped. Explored timelines report back through the shared
assertion table, which carries no metric values.

## Beyond Prometheus

`PrometheusSource` is one implementation of `MetricsSource`, a registry-agnostic
trait in `moonpool-core`:

```rust,ignore
pub trait MetricsSource: Send + Sync + 'static {
    fn collect(&self) -> Vec<MetricSample>;
    fn set_clock(&self, clock: Arc<dyn MetricClock>) {}
    fn series(&self) -> BTreeMap<String, Vec<MetricPoint>> { BTreeMap::new() }
}
```

`collect` alone is enough for a scrape-only adapter over any registry. The other
two are what an adapter implements to deliver an exact time series. An
OpenTelemetry adapter slots in the same way, with no change to the simulation
runner.
