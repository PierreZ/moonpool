//! A metered key/value service: the metrics story end to end.
//!
//! The process is instrumented the way a production service is — a request
//! counter split by outcome, a gauge for in-flight work, a latency histogram —
//! using nothing but a `prometheus::Registry`. Registering that registry with
//! the simulation is the only moonpool-specific step; everything else is the
//! instrumentation you would ship.
//!
//! What the simulation adds is a report: totals aggregated across every seed,
//! attributed per node, plus an exact time series of every mutation stamped on
//! the simulated clock — so a latency spike lines up with the partition that
//! caused it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_prometheus::{PrometheusSource, SimCounter, SimGauge, SimHistogram};
use moonpool_sim::{
    Process, RandomProvider, SimContext, SimulationError, SimulationResult, TaskProvider,
    TimeProvider, Workload, assert_always, assert_sometimes,
};

/// Latency buckets in seconds, bracketing what this service actually produces
/// (single-digit to ~100ms of simulated work). Prometheus' defaults are tuned
/// for real HTTP latency and would put every observation in one bucket here.
const LATENCY_BUCKETS: &[f64] = &[0.001, 0.005, 0.010, 0.025, 0.050, 0.100, 0.250];

/// The service's metrics, resolved once per boot from the node's registry.
///
/// Looked up rather than registered afresh: the source is keyed by node IP and
/// outlives reboots, so a process booting a second time finds its metrics
/// already there.
struct ServiceMetrics {
    served: SimCounter,
    rejected: SimCounter,
    in_flight: SimGauge,
    keys: SimGauge,
    latency: SimHistogram,
}

impl ServiceMetrics {
    fn resolve(source: &PrometheusSource) -> SimulationResult<Self> {
        let err = |e: prometheus::Error| SimulationError::InvalidState(format!("metrics: {e}"));
        Ok(Self {
            served: source
                .counter("kv_requests_served_total", "Requests served")
                .map_err(err)?,
            rejected: source
                .counter(
                    "kv_requests_rejected_total",
                    "Requests rejected when overloaded",
                )
                .map_err(err)?,
            in_flight: source
                .gauge("kv_requests_in_flight", "Requests currently being served")
                .map_err(err)?,
            keys: source
                .gauge("kv_keys_stored", "Keys currently held")
                .map_err(err)?,
            latency: source
                .histogram_with_buckets(
                    "kv_request_latency_seconds",
                    "Time to serve one request",
                    LATENCY_BUCKETS.to_vec(),
                )
                .map_err(err)?,
        })
    }
}

/// Beyond this many concurrent requests the node sheds load, which is what
/// makes `kv_requests_rejected_total` a metric worth watching.
const MAX_IN_FLIGHT: f64 = 3.0;

/// Concurrent request handlers per node. More than [`MAX_IN_FLIGHT`], so the
/// load-shedding path is actually exercised rather than being dead code.
const WORKERS_PER_NODE: usize = 5;

/// Distinct keys the workers cycle through.
const KEY_SPACE: u64 = 16;

/// A key/value node that meters everything it does.
///
/// The store is shared across the node's concurrent request handlers, which is
/// what lets in-flight requests overlap and the shed-load counter move.
#[derive(Default, Clone)]
pub struct MeteredKvNode {
    store: Arc<Mutex<BTreeMap<String, String>>>,
}

impl MeteredKvNode {
    /// Create an empty node.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Admission control: refuse the request when the node is already at its
    /// concurrency limit.
    ///
    /// Checked *before* the request is counted in-flight, so `MAX_IN_FLIGHT`
    /// means what it says.
    fn admit(metrics: &ServiceMetrics) -> bool {
        // Evaluated on every request, not just inside the shed branch: that is
        // what lets the assertion report *how often* the node saturates.
        let saturated = metrics.in_flight.get() >= MAX_IN_FLIGHT;
        assert_sometimes!(saturated, "kv node sheds load when saturated");
        if saturated {
            metrics.rejected.inc();
            return false;
        }
        metrics.in_flight.inc();
        true
    }

    /// Serve one admitted write, metering it exactly as production would.
    fn serve(&self, metrics: &ServiceMetrics, key: String, value: String) {
        let mut store = self
            .store
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        store.insert(key, value);
        metrics.served.inc();
        // A gauge that tracks real state: it must never disagree with the map.
        let stored = u32::try_from(store.len()).unwrap_or(u32::MAX);
        metrics.keys.set(f64::from(stored));
    }
}

#[async_trait]
impl Process for MeteredKvNode {
    fn name(&self) -> &'static str {
        "metered-kv"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let source = ctx.metrics::<PrometheusSource>().ok_or_else(|| {
            SimulationError::InvalidState("no metrics factory registered".to_owned())
        })?;
        let metrics = ServiceMetrics::resolve(&source)?;

        let metrics = Arc::new(metrics);
        let mut handles = Vec::with_capacity(WORKERS_PER_NODE);
        for worker in 0..WORKERS_PER_NODE {
            let node = self.clone();
            let metrics = metrics.clone();
            let time = ctx.time().clone();
            let random = ctx.random().clone();
            let shutdown = ctx.shutdown().clone();
            handles.push(
                ctx.task()
                    .spawn_task(&format!("kv-worker-{worker}"), async move {
                        let mut counter = u64::try_from(worker).unwrap_or(0);
                        let stride = u64::try_from(WORKERS_PER_NODE).unwrap_or(1);
                        while !shutdown.is_cancelled() {
                            if !MeteredKvNode::admit(&metrics) {
                                // Shed: back off before trying again, as a client
                                // seeing a 503 would.
                                time.sleep(Duration::from_millis(10)).await.ok();
                                continue;
                            }

                            // Timed on the simulated clock: `prometheus`' own
                            // start_timer() reads the wall clock, which would
                            // record host noise here and differ on every replay of
                            // the same seed.
                            let timer = metrics.latency.start_timer(&time);

                            let work_ms = random.random_range(1..40);
                            time.sleep(Duration::from_millis(work_ms)).await.ok();
                            node.serve(
                                &metrics,
                                format!("key-{}", counter % KEY_SPACE),
                                format!("value-{counter}"),
                            );

                            timer.stop_and_record();
                            metrics.in_flight.dec();
                            counter += stride;

                            time.sleep(Duration::from_millis(5)).await.ok();
                        }
                    }),
            );
        }

        for handle in handles {
            handle.await.ok();
        }

        assert_always!(
            metrics.in_flight.get() >= 0.0,
            "in-flight gauge never goes negative"
        );
        Ok(())
    }
}

/// Drives the nodes for a while, then checks the metrics tell a coherent story.
#[derive(Default)]
pub struct MeteredKvWorkload;

#[async_trait]
impl Workload for MeteredKvWorkload {
    fn name(&self) -> &'static str {
        "metered-kv-driver"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time().sleep(Duration::from_secs(5)).await.ok();
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Metrics are readable from the workload too, which is how a test
        // asserts on what the system under test recorded about itself.
        let handle = ctx.metrics_handle();
        let samples = handle.collect_all();
        assert_always!(
            !samples.is_empty(),
            "the nodes recorded something over five simulated seconds"
        );

        let series = handle.collect_series();
        assert_sometimes!(
            series.keys().any(|k| k.starts_with("kv_request_latency")),
            "latency observations are recorded as a time series"
        );
        Ok(())
    }
}
