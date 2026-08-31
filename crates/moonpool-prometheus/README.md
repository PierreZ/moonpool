# moonpool-prometheus

Scrape a `prometheus::Registry` into a moonpool simulation report.

Register a source per simulated node and the counters, gauges and histograms
your application already keeps show up as workload metrics, aggregated across
seeds:

```rust,ignore
SimulationBuilder::new()
    .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
    .processes(3, || Box::new(MyNode::new()))
    .workload(MyWorkload::default())
    .run();
```

See the [moonpool book](https://pierrez.github.io/moonpool/) for the full guide.
