//! Per-node application metrics registered on the simulation builder.
//!
//! A simulation runs every node in one OS process, so a single shared metrics
//! registry would merge all nodes' counters into one number. Instead the
//! builder takes a *factory* keyed by IP
//! ([`SimulationBuilder::metrics_factory`](super::builder::SimulationBuilder::metrics_factory)):
//! each process and workload gets its own [`MetricsSource`], reachable from
//! its [`SimContext`](super::context::SimContext) via
//! [`SimContext::metrics`](super::context::SimContext::metrics).
//!
//! The factory also gives per-seed freshness. Most registries (a
//! `prometheus::Registry` among them) have no reset, so reusing one instance
//! would make seed 50 report the sum of fifty runs. A fresh
//! [`MetricsHandle`] is built for every iteration, which is the same reasoning
//! behind registering a fault injector through
//! [`fault_factory`](super::builder::SimulationBuilder::fault_factory) rather
//! than as an instance.
//!
//! Reboots are the other way round: a process that crashes and restarts gets a
//! fresh [`Process`](super::process::Process) instance but the *same* source,
//! because its IP is unchanged. Counters therefore survive reboots, which
//! matches what a real node's `/metrics` endpoint shows after a process
//! restart on the same host — and means adapters must tolerate a metric being
//! looked up again after a reboot rather than re-registered.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use moonpool_core::metrics::{MetricClock, MetricPoint, MetricSample, MetricsSource};

use crate::observability::{Clock, SimulationLayerHandle};

/// Label stamped onto every scraped sample, naming the node it came from.
///
/// `instance` is Prometheus' own label for the scraped target, so a simulated
/// node maps onto it directly. A sample that already carries an `instance`
/// label keeps the application's value.
pub const INSTANCE_LABEL: &str = "instance";

/// One node's source, kept twice: once as the trait object the runner scrapes,
/// once as `Any` so [`MetricsHandle::get`] can hand the concrete type back to
/// application code.
#[derive(Clone)]
struct SourceEntry {
    source: Arc<dyn MetricsSource>,
    concrete: Arc<dyn Any + Send + Sync>,
}

/// Builds one [`MetricsSource`] per node IP.
///
/// Produced by [`SimulationBuilder::metrics_factory`](super::builder::SimulationBuilder::metrics_factory);
/// not constructed directly.
pub(crate) type SourceFactory = Box<dyn Fn(&str) -> SourceEntryPair>;

/// A source in both of the forms the runner needs. Opaque; see [`SourceFactory`].
pub(crate) type SourceEntryPair = (Arc<dyn MetricsSource>, Arc<dyn Any + Send + Sync>);

/// Shared, IP-keyed access to the simulation's application metrics.
///
/// Cheap to clone (`Arc`-based); every clone sees the same sources. One handle
/// is built per iteration and handed to every process and workload context.
#[derive(Clone, Default)]
pub struct MetricsHandle {
    sources: Arc<RwLock<BTreeMap<String, SourceEntry>>>,
}

impl MetricsHandle {
    /// Create an empty handle — the no-metrics-configured case.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a handle holding one source per IP, arming each source's
    /// event-driven recording against the simulated clock.
    ///
    /// `clock` is the observability layer's sim-time handle, which the
    /// orchestrator advances after every `sim.step()`. Timestamping from it
    /// rather than the wall clock is what makes a recorded series replay
    /// identically for a given seed.
    pub(crate) fn from_factory<'a>(
        factory: &SourceFactory,
        ips: impl IntoIterator<Item = &'a str>,
        clock: &SimulationLayerHandle,
    ) -> Self {
        let handle = Self::new();
        let clock: Arc<dyn MetricClock> = Arc::new(SimClock(clock.clone()));
        {
            let mut sources = handle.write();
            for ip in ips {
                let (source, concrete) = factory(ip);
                source.set_clock(clock.clone());
                sources.insert(ip.to_owned(), SourceEntry { source, concrete });
            }
        }
        handle
    }

    /// Whether any source is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// Get the source registered for `ip`, downcast to its concrete type.
    ///
    /// Returns `None` when no metrics factory was configured, when `ip` is not
    /// a simulated node, or when `S` is not the type the factory produced.
    #[must_use]
    pub fn get<S: MetricsSource>(&self, ip: &str) -> Option<Arc<S>> {
        self.read()
            .get(ip)
            .and_then(|entry| entry.concrete.clone().downcast::<S>().ok())
    }

    /// Scrape every source, stamping each sample with its node's
    /// [`INSTANCE_LABEL`].
    ///
    /// Samples come back sorted by [`MetricSample::sort_key`] so that two
    /// replays of a seed produce identical reports regardless of what order
    /// the adapters emitted them in.
    #[must_use]
    pub fn collect_all(&self) -> Vec<MetricSample> {
        let mut all: Vec<MetricSample> = Vec::new();
        // BTreeMap iteration is ordered by IP, so the scrape itself is stable.
        for (ip, entry) in self.read().iter() {
            for mut sample in entry.source.collect() {
                sample.label_if_absent(INSTANCE_LABEL, ip);
                all.push(sample);
            }
        }
        all.sort_by_cached_key(MetricSample::sort_key);
        all
    }

    /// Collect every node's recorded series, keyed by
    /// `instance` + series identity and ascending in time.
    ///
    /// Unlike [`collect_all`](Self::collect_all), which is a single snapshot,
    /// these are pushed by instrumented handles as the application mutates
    /// them — one point per `inc()` / `set()` / `observe()`, at the simulated
    /// instant it happened. That is what makes them plottable as a time
    /// series.
    #[must_use]
    pub fn collect_series(&self) -> BTreeMap<String, Vec<MetricPoint>> {
        let mut all = BTreeMap::new();
        for (ip, entry) in self.read().iter() {
            for (key, points) in entry.source.series() {
                all.insert(qualify_series_key(&key, ip), points);
            }
        }
        all
    }

    /// Total points dropped across nodes because a series hit its capacity.
    #[must_use]
    pub fn dropped_points(&self) -> u64 {
        self.read()
            .values()
            .map(|entry| entry.source.dropped_points())
            .sum()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, SourceEntry>> {
        self.sources
            .read()
            .expect("RwLock poisoned: prior task panicked")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, SourceEntry>> {
        self.sources
            .write()
            .expect("RwLock poisoned: prior task panicked")
    }
}

impl std::fmt::Debug for MetricsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsHandle")
            .field("nodes", &self.read().len())
            .finish()
    }
}

/// Insert the [`INSTANCE_LABEL`] into a recorded series' key.
///
/// Recorded keys come from the source, which knows nothing about which node it
/// belongs to, so the IP is spliced in here — inside the brace group when the
/// key already has labels, as a new group otherwise. The result matches the
/// key a scraped [`MetricSample`] produces for the same series.
fn qualify_series_key(key: &str, ip: &str) -> String {
    let instance = format!("{INSTANCE_LABEL}=\"{ip}\"");
    match key.split_once('{') {
        // Already labelled by the application: keep its labels and add ours.
        Some((name, rest)) if rest.ends_with('}') => {
            let inner = &rest[..rest.len() - 1];
            if inner.contains(&format!("{INSTANCE_LABEL}=")) {
                key.to_owned()
            } else {
                let mut labels: Vec<&str> = inner.split(',').filter(|s| !s.is_empty()).collect();
                labels.push(&instance);
                labels.sort_unstable();
                format!("{name}{{{}}}", labels.join(","))
            }
        }
        _ => format!("{key}{{{instance}}}"),
    }
}

/// Adapts the observability layer's sim clock to [`MetricClock`].
struct SimClock(SimulationLayerHandle);

impl MetricClock for SimClock {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonpool_core::metrics::{MetricValue, u64_to_f64_exact};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Counting {
        hits: AtomicU64,
    }

    impl MetricsSource for Counting {
        fn collect(&self) -> Vec<MetricSample> {
            let hits = u64_to_f64_exact(self.hits.load(Ordering::Relaxed));
            vec![MetricSample::new(
                "hits_total",
                Vec::new(),
                MetricValue::Counter(hits),
            )]
        }
    }

    struct Other;

    impl MetricsSource for Other {
        fn collect(&self) -> Vec<MetricSample> {
            Vec::new()
        }
    }

    fn handle_for(ips: &[&str]) -> (MetricsHandle, crate::observability::InstallGuard) {
        let factory: SourceFactory = Box::new(|_ip| {
            let s = Arc::new(Counting {
                hits: AtomicU64::new(0),
            });
            (
                s.clone() as Arc<dyn MetricsSource>,
                s as Arc<dyn Any + Send + Sync>,
            )
        });
        let (obs, guard) = crate::observability::SimulationLayer::new().install();
        (
            MetricsHandle::from_factory(&factory, ips.iter().copied(), &obs),
            guard,
        )
    }

    #[test]
    fn empty_handle_collects_nothing() {
        let handle = MetricsHandle::new();
        assert!(handle.is_empty());
        assert!(handle.collect_all().is_empty());
        assert!(handle.get::<Counting>("10.0.1.1").is_none());
    }

    #[test]
    fn factory_builds_one_source_per_ip() {
        let (handle, _guard) = handle_for(&["10.0.1.1", "10.0.1.2"]);
        assert!(!handle.is_empty());

        let a = handle.get::<Counting>("10.0.1.1").expect("source for .1");
        a.hits.fetch_add(3, Ordering::Relaxed);

        let samples = handle.collect_all();
        assert_eq!(samples.len(), 2, "one series per node");
        assert_eq!(
            samples[0].sort_key(),
            r#"hits_total{instance="10.0.1.1"}"#,
            "sorted, and stamped with the node IP"
        );
        assert_eq!(samples[0].value, MetricValue::Counter(3.0));
        assert_eq!(
            samples[1].value,
            MetricValue::Counter(0.0),
            "the other node has its own registry"
        );
    }

    #[test]
    fn get_with_wrong_type_returns_none() {
        let (handle, _guard) = handle_for(&["10.0.1.1"]);
        assert!(handle.get::<Other>("10.0.1.1").is_none());
        assert!(handle.get::<Counting>("10.0.9.9").is_none(), "unknown ip");
    }

    #[test]
    fn qualify_series_key_adds_the_instance_label() {
        assert_eq!(
            qualify_series_key("hits_total", "10.0.1.1"),
            r#"hits_total{instance="10.0.1.1"}"#
        );
        assert_eq!(
            qualify_series_key(r#"hits_total{code="200"}"#, "10.0.1.1"),
            r#"hits_total{code="200",instance="10.0.1.1"}"#,
            "existing labels are kept and the result stays sorted"
        );
        assert_eq!(
            qualify_series_key(r#"hits_total{instance="mine"}"#, "10.0.1.1"),
            r#"hits_total{instance="mine"}"#,
            "an application-set instance label wins"
        );
    }

    #[test]
    fn collect_all_is_sorted_regardless_of_insertion_order() {
        let (first, _g1) = handle_for(&["10.0.1.2", "10.0.1.1"]);
        let (second, _g2) = handle_for(&["10.0.1.1", "10.0.1.2"]);
        let keys = |h: &MetricsHandle| {
            h.collect_all()
                .iter()
                .map(MetricSample::sort_key)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&first), keys(&second));
    }
}
