# moonpool-prometheus

Report the `prometheus` metrics your code already keeps in a moonpool
simulation report.

Register a `PrometheusSource` per simulated node. The counters, gauges and
histograms your application records appear in the report, per node and
aggregated across seeds. The application records them the same way as in
production.

## Quick start

```rust
// In the test: one source per node, keyed by IP so it survives reboots.
let report = SimulationBuilder::new()
    .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
    .processes(3, || Box::new(MyNode::new()))
    .workload(MyWorkload::default())
    .run()?;

// In the process or workload: get-or-register, then record as usual.
let metrics = ctx.metrics::<PrometheusSource>().expect("factory registered");
let requests = metrics.counter("requests_total", "Requests processed")?;
let latency = metrics.histogram("request_latency_seconds", "Request latency")?;
let timer = latency.start_timer(ctx.time()); // simulated clock, replays exactly
requests.inc();
timer.stop_and_record();
```

## Features

| Item | Role |
|---|---|
| `PrometheusSource` | A `prometheus::Registry` that implements moonpool's `MetricsSource` |
| `counter`, `gauge`, `histogram`, `histogram_with_buckets`, `histogram_with_opts` | Get or register a metric; returns an instrumented handle |
| `counter_vec`, `gauge_vec`, `histogram_vec` | Labelled families |
| `SimCounter`, `SimGauge`, `SimHistogram` (and `Vec` variants) | Wrap the real metric (`inner()`) and record every mutation |
| `SimHistogram::start_timer` / `SimTimer` | A latency timer on the provider clock, not the wall clock |
| `set_series_capacity` | Cap the points kept per series |
| `registry` | The wrapped registry, for a production `/metrics` handler |

Metrics reach the report two ways:

- **Instrumented handles push.** Every `inc()`, `set()` and `observe()` is one
  point in a time series, stamped with the simulated clock
  (`SimulationMetrics::app_series`).
- **A final scrape catches the rest.** Metrics registered by other code are
  read once at the end of the iteration (`SimulationMetrics::app_metrics`).

Use `SimHistogram::start_timer`, not `prometheus`' own `start_timer()`. The
`prometheus` timer reads the wall clock, so it records host noise and differs
on every replay of the same seed.

## Feature flags

None. The crate depends on `moonpool-core` only, never on `moonpool-sim`, so
it also works in production builds. The facade crate exposes it as
`moonpool::prometheus` behind its `prometheus` feature.

## Example

`crates/moonpool-sim-examples/src/metrics_service.rs` instruments a key/value
service and runs it under chaos:

```sh
cargo xtask sim run metrics-service
```

## Documentation

- [API documentation](https://docs.rs/moonpool-prometheus)
- [Application Metrics](https://pierrez.github.io/moonpool/part3-building/25-metrics.html) in the Sim Book
