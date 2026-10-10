//! Simulation builder pattern for configuring and running experiments.
//!
//! This module provides the main `SimulationBuilder` type for setting up
//! and executing simulation experiments.

use std::time::Duration;

use tracing::instrument;

use crate::SimulationError;
use crate::observability::{Invariant, TraceQuery};
use crate::runner::fault_injector::FaultInjector;
use crate::runner::locality::LocalityConfig;
use crate::runner::process::{Attrition, Process};
use crate::runner::tags::TagDistribution;
use crate::runner::workload::Workload;

use super::app_metrics::SourceFactory;
pub use super::config::{Chaos, ChaosMode, IterationControl, ProcessCount, WorkloadCount};
use moonpool_core::metrics::MetricsSource;
use moonpool_core::metrics::query::MetricQueryPlan;

/// Client identity information for a single workload instance.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkloadClientInfo {
    /// The resolved client ID for this instance.
    pub(crate) client_id: usize,
    /// Total number of workload instances sharing this builder entry.
    pub(crate) client_count: usize,
}

/// Internal storage for one process group in the builder: a `.processes()` or
/// `.cluster()` registration.
pub(crate) struct ProcessEntry {
    pub(crate) count: ProcessCount,
    pub(crate) factory: Box<dyn Fn() -> Box<dyn Process>>,
    pub(crate) tags: TagDistribution,
    /// The group's name: its process type's [`Process::name`].
    pub(crate) name: String,
    /// Failure-domain topology. When `Some`, it determines the process count
    /// (sampled per seed) and `count` is ignored.
    pub(crate) locality: Option<LocalityConfig>,
}

impl ProcessEntry {
    /// Resolve this group's process count for the current seed, drawing from
    /// the sim RNG (already seeded) when the count or topology is a range.
    pub(super) fn resolve_shape(&self) -> (usize, Option<Vec<crate::LocalityInfo>>) {
        // When a topology is configured it owns the process count (sampled per
        // seed); otherwise fall back to the flat `.processes()` count.
        let localities = self.locality.as_ref().map(LocalityConfig::resolve_topology);
        let count = localities
            .as_ref()
            .map_or_else(|| self.count.resolve(), Vec::len);
        (count, localities)
    }
}

/// The IP of process `index` (0-based) in process group `group` (0-based).
///
/// Every group owns its own `/24`: the first group registered is
/// `10.0.1.{1..=N}`, the second `10.0.2.{1..=N}`, and so on, so a group's
/// members are contiguous and a single-group builder keeps its historical
/// addresses. Workloads live on `10.0.0.x`.
pub(super) fn process_ip(group: usize, index: usize) -> String {
    assert!(
        group < 255,
        "at most 255 process groups fit the 10.0.{{group}}.x address plan (got group #{group})"
    );
    assert!(
        index < 255,
        "at most 255 processes per group fit the 10.0.{{group}}.x address plan (got index {index})"
    );
    format!("10.0.{}.{}", group + 1, index + 1)
}

/// Internal storage for workload entries in the builder.
pub(super) enum WorkloadEntry {
    /// Single instance, reused across iterations (from `.workload()`).
    Instance(Option<Box<dyn Workload>>),
    /// Factory-based, fresh instances per iteration (from `.workloads()`).
    Factory {
        count: WorkloadCount,
        factory: Box<dyn Fn(usize) -> Box<dyn Workload>>,
    },
}

/// Builder pattern for configuring and running simulation experiments.
pub struct SimulationBuilder {
    pub(super) iteration_control: IterationControl,
    pub(super) entries: Vec<WorkloadEntry>,
    /// Server-process groups in registration order; each gets its own IP range.
    pub(super) process_entries: Vec<ProcessEntry>,
    /// Built-in attrition regimes, one injector each, with their per-seed
    /// sampling mode. Several entries let each process group (or tag) carry
    /// its own reboot regime and `max_dead` budget.
    pub(super) attritions: Vec<(Attrition, ChaosMode)>,
    /// Correlated group outages, one injector each, with their per-seed
    /// sampling mode.
    pub(super) outages: Vec<(crate::runner::outage::Outage, ChaosMode)>,
    pub(super) seeds: Vec<u64>,
    pub(super) network_chaos: Option<ChaosMode>,
    pub(super) storage_chaos: Option<ChaosMode>,
    /// Replicated storage fault patterns, drawn per seed over the topology.
    pub(super) replicated_faults: Vec<crate::storage::ReplicatedFaults>,
    /// Deterministic allow-mask applied after each per-seed network profile.
    pub(super) network_fault_mask: crate::NetworkFaultMask,
    /// Storage fault families retained after the storage profile is sampled.
    pub(super) storage_fault_mask: crate::StorageFaultMask,
    /// Distance-based link latency, applied to every iteration's network config.
    pub(super) link_latency: Option<crate::network::LinkLatencyConfig>,
    /// End-to-end byte window per stream direction, applied to every
    /// iteration's network config; `None` keeps the default.
    pub(super) tcp_send_window_bytes: Option<usize>,
    /// Maximum unaccepted connections per listener; `None` keeps the default.
    pub(super) accept_backlog_capacity: Option<usize>,
    /// Buggify-driven knob value-perturbation, enabled via [`Chaos::BuggifyKnobs`].
    /// Internal flag (not a public builder method) so the opt-in stays inside the
    /// `enable_chaos`/`Chaos` model.
    pub(super) buggify_knobs: bool,
    pub(super) swarm_operations: bool,
    /// Run every seed twice and fail it when the second run's draw
    /// fingerprints differ from the first's (see
    /// [`SimulationBuilder::check_determinism`]).
    pub(super) check_determinism: bool,
    /// The trace floor for the run's subscriber and timeline (see
    /// [`SimulationBuilder::trace_level`]). `INFO` by default.
    pub(super) trace_level: tracing::level_filters::LevelFilter,
    pub(super) invariants: Vec<Box<dyn Invariant + Send>>,
    /// Factories that build a fresh fault injector for every root and explored
    /// timeline, mirroring `workload_factory`.
    pub(super) fault_factories: Vec<Box<dyn Fn() -> Box<dyn FaultInjector> + 'static>>,
    /// Builds one application-metrics source per node IP, rebuilt each
    /// iteration so counters start from zero on every seed.
    pub(super) metrics_factory: Option<SourceFactory>,
    /// Metric queries the runner evaluates against every successful seed.
    pub(super) metric_queries: Vec<MetricQueryPlan>,
    pub(super) chaos_duration: Option<Duration>,
    pub(super) exploration_config: Option<crate::chaos::exploration_glue::ExplorationConfig>,
    /// Replay breakpoints staged for the next orchestration run. Installed
    /// *after* `SimWorld` construction (whose RNG reset would clear them);
    /// set by the exploration phase before executing a job.
    pub(super) pending_replay: Option<Vec<(u64, u64)>>,
    /// Recipe installed for every iteration (set by [`Self::replay_timeline`]).
    pub(super) replay_recipe: Option<Vec<(u64, u64)>>,
    pub(super) run_time_budget: Duration,
}

impl Default for SimulationBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulationBuilder {
    /// Create a new empty simulation builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            iteration_control: IterationControl::UntilCoverageStable {
                plateau_seeds: 10,
                max_iterations: 1000,
            },
            entries: Vec::new(),
            process_entries: Vec::new(),
            attritions: Vec::new(),
            outages: Vec::new(),
            seeds: Vec::new(),
            network_chaos: None,
            storage_chaos: None,
            replicated_faults: Vec::new(),
            network_fault_mask: crate::NetworkFaultMask::all(),
            storage_fault_mask: crate::StorageFaultMask::all(),
            link_latency: None,
            tcp_send_window_bytes: None,
            accept_backlog_capacity: None,
            buggify_knobs: false,
            swarm_operations: false,
            check_determinism: false,
            trace_level: tracing::level_filters::LevelFilter::INFO,
            invariants: Vec::new(),
            fault_factories: Vec::new(),
            metrics_factory: None,
            metric_queries: Vec::new(),
            chaos_duration: None,
            exploration_config: None,
            pending_replay: None,
            replay_recipe: None,
            run_time_budget: super::stall::DEFAULT_RUN_TIME_BUDGET,
        }
    }

    /// Add a single workload instance to the simulation.
    ///
    /// The instance is reused across iterations (the `run()` method is called
    /// each iteration on the same struct). Gets `client_id = 0`, `client_count = 1`.
    /// This form is intentionally rejected when exploration is enabled because
    /// an arbitrary trait object cannot be reconstructed with fresh state for
    /// each continuation. Use [`Self::workload_factory`] or [`Self::workloads`]
    /// for exploration.
    #[must_use]
    pub fn workload(mut self, w: impl Workload) -> Self {
        self.entries
            .push(WorkloadEntry::Instance(Some(Box::new(w))));
        self
    }

    /// Add one workload reconstructed from a factory for every timeline.
    ///
    /// Unlike [`Self::workload`], this never reuses a previously-run workload
    /// value. Use this form for exploration so every root and continuation
    /// timeline starts with fresh test-driver state and captured bug recipes
    /// can be replayed from the same lifecycle boundary.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// builder.workload_factory(|| Box::new(ClientWorkload::new()))
    /// ```
    #[must_use]
    pub fn workload_factory(mut self, factory: impl Fn() -> Box<dyn Workload> + 'static) -> Self {
        self.entries.push(WorkloadEntry::Factory {
            count: WorkloadCount::Fixed(1),
            factory: Box::new(move |_| factory()),
        });
        self
    }

    /// Add a group of server processes to the simulation.
    ///
    /// Processes represent the **system under test** — they can be killed and
    /// restarted (rebooted). A fresh instance is created from the factory on
    /// every boot.
    ///
    /// The `count` parameter accepts either a fixed `usize` or a
    /// `RangeInclusive<usize>` for seeded random count per iteration.
    ///
    /// # Process groups
    ///
    /// Each call registers one independent **group**, named after its process
    /// type ([`Process::name`]), with its own factory and its own per-seed
    /// count draw. Call it once per role of the system under test — acceptors
    /// and matchmakers, a primary tier and a spare pool — and each role's
    /// cluster size varies per seed independently of the others'. Groups draw
    /// their counts in registration order, so a seed replays the same shape.
    ///
    /// Every group owns its own IP range: the first group registered is
    /// `10.0.1.{1..=N}`, the second `10.0.2.{1..=N}`, and so on, so a group's
    /// members are contiguous. Inside the simulation,
    /// [`WorkloadTopology::ips_in_group`](crate::WorkloadTopology::ips_in_group)
    /// and [`FaultContext::ips_in_group`](crate::FaultContext::ips_in_group)
    /// answer the group by name, and a process's
    /// [`client_id`](crate::SimContext::client_id) /
    /// [`client_count`](crate::SimContext::client_count) are its index within
    /// and the size of its own group. [`Attrition::victims`](crate::Attrition::victims)
    /// keeps the built-in attrition on one group.
    ///
    /// # Panics
    ///
    /// Panics when a group with the same process name was already registered:
    /// the name is the group's identity, so each role needs its own process
    /// type (or a distinct [`Process::name`]).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Fixed 3 server processes
    /// builder.processes(3, || Box::new(MyNode::new()))
    ///
    /// // 3 to 7 processes, randomized per iteration
    /// builder.processes(3..=7, || Box::new(MyNode::new()))
    ///
    /// // Two roles: 3–5 acceptors on 10.0.1.x, 0–3 matchmakers on 10.0.2.x
    /// builder
    ///     .processes(3..=5, || Box::new(Acceptor::new()))
    ///     .processes(0..=3, || Box::new(Matchmaker::new()))
    /// ```
    #[must_use]
    pub fn processes(
        self,
        count: impl Into<ProcessCount>,
        factory: impl Fn() -> Box<dyn Process> + 'static,
    ) -> Self {
        self.register_group(count.into(), factory, None)
    }

    /// Push one process group, rejecting a duplicate group name.
    fn register_group(
        mut self,
        count: ProcessCount,
        factory: impl Fn() -> Box<dyn Process> + 'static,
        locality: Option<LocalityConfig>,
    ) -> Self {
        let sample = factory();
        let name = sample.name().to_string();
        drop(sample);
        assert!(
            !self.process_entries.iter().any(|entry| entry.name == name),
            "process group {name:?} is already registered: each .processes() / .cluster() call \
             is one group, named after its process type, so give each role its own name"
        );
        self.process_entries.push(ProcessEntry {
            count,
            factory: Box::new(factory),
            tags: TagDistribution::new(),
            name,
            locality,
        });
        self
    }

    /// Register a group of server processes laid out across a failure-domain
    /// topology.
    ///
    /// Unlike [`processes`](Self::processes), the [`LocalityConfig`] *is* the
    /// spawn spec: it determines the process count (sampled per seed), assigns
    /// each process a datacenter / zone / machine, and lets machine- and
    /// zone-scoped attrition reboot collocated processes together. Like
    /// `.processes()`, each call registers one independent group with its own
    /// IP range, and the two may be mixed on one builder.
    ///
    /// Tags ([`tags`](Self::tags)) remain orthogonal and may still be chained.
    ///
    /// # Panics
    ///
    /// Panics when a group with the same process name was already registered.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // 3 datacenters × 3 zones × 3 machines × 1 process = 27 processes,
    /// // with the datacenter count randomized per seed.
    /// builder.cluster(
    ///     LocalityConfig::new(1..=3, 3, 3, 1),
    ///     || Box::new(MyNode::new()),
    /// )
    /// ```
    #[must_use]
    pub fn cluster(
        self,
        config: LocalityConfig,
        factory: impl Fn() -> Box<dyn Process> + 'static,
    ) -> Self {
        // `count` is unused when locality is present; the topology decides it.
        self.register_group(ProcessCount::Fixed(0), factory, Some(config))
    }

    /// Give links a distance-dependent latency, resolved through the
    /// [`.cluster()`](Self::cluster) topology.
    ///
    /// Realism rather than chaos: a cross-datacenter hop stays slow even on a
    /// healthy seed. Each ordered IP pair samples its class distribution once at
    /// first contact and keeps it for the run. Pairs where either side has no
    /// locality (workload clients, plain `.processes()` runs) are unaffected.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// SimulationBuilder::new()
    ///     .cluster(LocalityConfig::new(2, 2, 2, 1), || Box::new(MyNode::new()))
    ///     .link_latency(LinkLatencyConfig::default())
    /// ```
    #[must_use]
    pub fn link_latency(mut self, config: crate::network::LinkLatencyConfig) -> Self {
        self.link_latency = Some(config);
        self
    }

    /// Set the end-to-end byte window of every stream direction (see
    /// [`NetworkConfiguration::tcp_send_window_bytes`](crate::NetworkConfiguration::tcp_send_window_bytes)).
    ///
    /// A small window makes a slow reader back its writer up early, which is
    /// how flow-control bugs (a server that keeps streaming into a client
    /// that stopped reading, a pipeline that deadlocks on its own replies)
    /// become reachable in a short run. Deployment shape rather than chaos:
    /// applied verbatim on every seed and consumes no draws.
    #[must_use]
    pub fn tcp_send_window_bytes(mut self, bytes: usize) -> Self {
        self.tcp_send_window_bytes = Some(bytes);
        self
    }

    /// Set the maximum unaccepted TCP connections per listener for every seed.
    ///
    /// A full listener backlog makes `connect()` wait until an `accept()`
    /// returns a connection. This deployment-shape setting consumes no RNG
    /// draws. `capacity` must be greater than zero.
    #[must_use]
    #[instrument(skip(self))]
    pub fn accept_backlog_capacity(mut self, capacity: usize) -> Self {
        assert!(capacity > 0, "accept backlog capacity must be positive");
        self.accept_backlog_capacity = Some(capacity);
        self
    }

    /// Spread random storage damage over a process group's failure domains
    /// so no replicated record is ever damaged everywhere.
    ///
    /// Each seed draws a [`FaultPattern`](crate::FaultPattern) over the
    /// domains at `config`'s level of the [`.cluster()`](Self::cluster)
    /// topology, among the members of `config`'s
    /// [group](crate::ReplicatedFaults::group) (every process without one):
    /// a **minority** (only a few domains take damage), a **stripe
    /// rotation** (every domain does, each replicated record only in the
    /// domains its key rotates to), or a **rolling turn** (one domain at a
    /// time takes damage anywhere, the turn moving on once its disks show
    /// the damage repaired). Processes describe their disks with
    /// [`SimStorageProvider::publish_layout`](crate::SimStorageProvider::publish_layout):
    /// each region's `stripe` is the record's key, shared by every
    /// replica's copy. A record holds damage in at most `tolerance` domains
    /// at once, and so does node-local data, by construction.
    ///
    /// Call it once per replicated group: each call draws its own pattern
    /// and runs its own turn. Processes outside every covered group keep
    /// plain storage chaos.
    ///
    /// It shapes the families [`Chaos::Storage`] enables, the same ones a
    /// [`FaultFocus`](crate::FaultFocus) weighs; it is inert without storage
    /// chaos. Not covered: disk failure, wipes, and namespace faults (a lost
    /// unsynced directory entry), which take a whole disk or only unsynced
    /// state.
    ///
    /// # Example
    ///
    /// ```ignore
    /// SimulationBuilder::new()
    ///     .cluster(LocalityConfig::new(1, 3, 1..=2, 1), make_node)
    ///     .enable_chaos([Chaos::Storage(ChaosMode::Swarm)])
    ///     .replicated_storage_faults(ReplicatedFaults::new(DomainLevel::Zone).group("node"))
    /// ```
    #[must_use]
    pub fn replicated_storage_faults(mut self, config: crate::storage::ReplicatedFaults) -> Self {
        self.replicated_faults.push(config);
        self
    }

    /// Retain only the selected network fault families in every per-seed
    /// Random or Swarm profile.
    ///
    /// The mask is applied after profile sampling and buggify knob
    /// perturbation, immediately before the [`SimWorld`](crate::SimWorld) is
    /// created. Applying it consumes no randomness, so configuration RNG draw
    /// order, exploration recipes, and replay remain unchanged. The default
    /// mask retains every family and is behaviorally inert.
    ///
    /// # Example
    ///
    /// ```ignore
    /// SimulationBuilder::new()
    ///     .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
    ///     .network_fault_mask(
    ///         NetworkFaultMask::all().without(NetworkFault::BitFlip),
    ///     )
    ///     .enable_exploration(exploration_config)
    /// ```
    #[must_use]
    pub fn network_fault_mask(mut self, mask: crate::NetworkFaultMask) -> Self {
        self.network_fault_mask = mask;
        self
    }

    /// Restrict the storage fault families a sampled storage profile keeps,
    /// the storage twin of [`network_fault_mask`](Self::network_fault_mask).
    ///
    /// Applied after profile sampling and buggify knob perturbation,
    /// immediately before the [`SimWorld`](crate::SimWorld) is created; it
    /// consumes no randomness, so draw order, exploration recipes and replay
    /// are unchanged. The default mask retains every family.
    ///
    /// # Example
    ///
    /// ```ignore
    /// SimulationBuilder::new()
    ///     .enable_chaos([Chaos::Storage(ChaosMode::Swarm)])
    ///     .storage_fault_mask(
    ///         StorageFaultMask::all().without(StorageFault::DiskFailure),
    ///     )
    /// ```
    #[must_use]
    pub fn storage_fault_mask(mut self, mask: crate::StorageFaultMask) -> Self {
        self.storage_fault_mask = mask;
        self
    }

    /// Attach tag distribution to the last `.processes()` / `.cluster()` call.
    ///
    /// Tags are distributed round-robin across that group's process instances
    /// (each group's round-robin starts over). Each tag dimension is
    /// distributed independently.
    ///
    /// # Errors
    ///
    /// Returns `SimulationError::InvalidState` if called without a preceding
    /// `.processes()` / `.cluster()` call.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // 5 processes: dc cycles east/west/eu, rack cycles r1/r2
    /// builder.processes(5, || Box::new(MyNode::new()))
    ///     .tags(&[
    ///         ("dc", &["east", "west", "eu"]),
    ///         ("rack", &["r1", "r2"]),
    ///     ])?
    /// ```
    pub fn tags(mut self, dimensions: &[(&str, &[&str])]) -> Result<Self, SimulationError> {
        let entry = self.process_entries.last_mut().ok_or_else(|| {
            SimulationError::InvalidState("tags() must be called after processes()".into())
        })?;
        for (key, values) in dimensions {
            entry.tags.add(key, values);
        }
        Ok(self)
    }

    /// Add built-in attrition for automatic process reboots during the chaos
    /// phase, sampled as written every seed ([`ChaosMode::Random`]).
    ///
    /// Add multiple workload instances from a factory.
    ///
    /// The factory receives an instance index (0-based) and must return a fresh
    /// workload. Instances are created each iteration and dropped afterward.
    /// Client IDs default to sequential starting from 0 (FDB-style).
    ///
    /// The workload is responsible for its own `name()` — use the index to
    /// produce unique names when count > 1 (e.g., `format!("client-{i}")`).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // 3 fixed replicas
    /// builder.workloads(WorkloadCount::Fixed(3), |i| Box::new(ReplicaWorkload::new(i)))
    ///
    /// // 1–5 random clients
    /// builder.workloads(WorkloadCount::Random(1..6), |i| Box::new(ClientWorkload::new(i)))
    /// ```
    #[must_use]
    pub fn workloads(
        mut self,
        count: WorkloadCount,
        factory: impl Fn(usize) -> Box<dyn Workload> + 'static,
    ) -> Self {
        self.entries.push(WorkloadEntry::Factory {
            count,
            factory: Box::new(factory),
        });
        self
    }

    /// Set the trace floor for the run: the most verbose `tracing` level the
    /// simulation subscriber enables and the timeline captures.
    ///
    /// `INFO` (the default) is the sweep setting: invariants read `INFO`+
    /// events, and every `DEBUG`/`TRACE` span or event in process and workload
    /// code is disabled before the registry allocates it, so hot-path
    /// `#[tracing::instrument(level = "trace")]` costs one level compare.
    /// Lower it to `LevelFilter::DEBUG` or `LevelFilter::TRACE` when debugging
    /// one seed: those spans come alive and their events join the timeline,
    /// at the price of a slower run and a much larger capture. The floor
    /// never changes scheduling or randomness, so a seed replays identically
    /// at any level.
    #[must_use]
    pub fn trace_level(mut self, level: tracing::level_filters::LevelFilter) -> Self {
        self.trace_level = level;
        self
    }

    /// Add an invariant to be checked after every simulation step.
    #[must_use]
    pub fn invariant<I: Invariant>(mut self, i: I) -> Self {
        self.invariants.push(Box::new(i));
        self
    }

    /// Add a closure-based invariant.
    #[must_use]
    pub fn invariant_fn(
        mut self,
        name: impl Into<String>,
        f: impl Fn(&dyn TraceQuery, u64) + Send + 'static,
    ) -> Self {
        self.invariants
            .push(crate::observability::invariant_fn(name, f));
        self
    }

    /// Register an application-metrics source per simulated node.
    ///
    /// `factory` is called once per node IP at the start of every iteration.
    /// Whatever it returns — a `PrometheusSource` from `moonpool-prometheus`,
    /// or any other [`MetricsSource`] implementation — is reachable from that
    /// node's code as [`SimContext::metrics`](super::context::SimContext::metrics),
    /// and is scraped after the check phase into
    /// [`SimulationMetrics::app_metrics`](super::report::SimulationMetrics::app_metrics).
    /// The report aggregates the samples across seeds and prints them.
    ///
    /// Per-node, because every simulated node shares one OS process: a single
    /// registry would merge all of their counters into one number. Per
    /// iteration, because most registries have no reset, so a reused instance
    /// would make seed 50 report the sum of fifty runs.
    ///
    /// ```ignore
    /// SimulationBuilder::new()
    ///     .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
    ///     .processes(3, || Box::new(MyNode::new()))
    ///     .workload(MyWorkload::default())
    ///     .run()?;
    /// ```
    ///
    /// Metric values are reported, never used to steer the simulation: a
    /// metric derived from the wall clock is not deterministic. Record
    /// durations from `ctx.time()` for a metric that replays identically.
    ///
    /// Under fork-based exploration, only root timelines are scraped —
    /// explored timelines report back through the shared assertion table,
    /// which carries no metric values.
    #[must_use]
    pub fn metrics_factory<S, F>(mut self, factory: F) -> Self
    where
        S: MetricsSource,
        F: Fn(&str) -> std::sync::Arc<S> + 'static,
    {
        self.metrics_factory = Some(Box::new(move |ip| {
            let source = factory(ip);
            // Kept twice: as the trait object the runner scrapes, and as `Any`
            // so `ctx.metrics::<S>()` can hand the concrete type back.
            (
                source.clone() as std::sync::Arc<dyn MetricsSource>,
                source as std::sync::Arc<dyn std::any::Any + Send + Sync>,
            )
        }));
        self
    }

    /// Declare a metric query the runner evaluates after every successful
    /// seed.
    ///
    /// The runner — not the report, not the CLI — decides what is worth
    /// watching. Queries are declared once, before `run()`, and their results
    /// travel with the report in
    /// [`SimulationReport::metric_queries`](super::report::SimulationReport::metric_queries),
    /// each row stamped with the seed that produced it so a bad number is
    /// immediately replayable.
    ///
    /// ```ignore
    /// use std::time::Duration;
    /// use moonpool_core::metrics::query::{Mean, MetricQuery, Percentile};
    ///
    /// SimulationBuilder::new()
    ///     .metrics_factory(|_ip| Arc::new(PrometheusSource::default()))
    ///     .metric(
    ///         MetricQuery::select("requests_total")
    ///             .label("operation", "write")
    ///             .rate()
    ///             .bucketize(Duration::from_secs(60), Mean)
    ///             .named("write_throughput"),
    ///     )
    ///     .metric(
    ///         MetricQuery::select("request_latency_seconds")
    ///             .bucketize(Duration::from_secs(60), Percentile(0.99))
    ///             .named("write_p99"),
    ///     )
    ///     .workload(MyWorkload::default())
    ///     .run()?;
    /// ```
    ///
    /// Queries read the same data the report already collects, so this needs a
    /// [`metrics_factory`](Self::metrics_factory); without one there are no
    /// series to select and every query reports zero runs.
    #[must_use]
    pub fn metric(mut self, query: MetricQueryPlan) -> Self {
        self.metric_queries.push(query);
        self
    }

    /// Add a fault injector reconstructed from a factory for every timeline.
    ///
    /// A fresh instance is built for every iteration and for every explored
    /// continuation timeline, so an injector never carries state across
    /// seeds and scripted fault sequences (crash → hold-down → milestone →
    /// restart) replay exactly from the root seed plus recipe. Stateful
    /// injectors that need to share something across iterations (a test's
    /// observation log, say) capture an `Arc` in the factory closure.
    ///
    /// Injectors run only inside the chaos window, so a fault factory
    /// requires [`Self::chaos_duration`]: [`Self::run`] returns
    /// [`SimulationError::InvalidConfiguration`] for a builder that registers
    /// one without it, rather than silently never running it.
    #[must_use]
    pub fn fault_factory(mut self, factory: impl Fn() -> Box<dyn FaultInjector> + 'static) -> Self {
        self.fault_factories.push(Box::new(factory));
        self
    }

    /// Set the chaos phase duration: the window in which Moonpool may inject
    /// *new* faults.
    ///
    /// Fault injectors run concurrently with the workloads for this long. When
    /// it elapses the runner ends chaos in one step: it cancels
    /// [`ctx.chaos_shutdown()`](crate::FaultContext::chaos_shutdown), so
    /// injector loops wind down, and calls
    /// [`SimWorld::enter_recovery_mode`](crate::SimWorld::enter_recovery_mode),
    /// which turns off every configuration-driven network, storage, and
    /// block-device fault family and heals the partitions the simulator is
    /// holding.
    ///
    /// # The contract
    ///
    /// The cutoff bounds *fault generation*, nothing else. The persistent
    /// consequences of faults already injected stay part of the simulated
    /// state: corrupted sectors, lost or misdirected writes, connections the
    /// application already saw close, processes already killed, and whatever
    /// state the system was left in. Finite effects already started — a disk
    /// stall or throttle episode, a clog, a delayed packet — keep their
    /// deadlines and expire naturally.
    ///
    /// So the cutoff does **not** mean the cluster is healthy. It means the
    /// environment stopped making it worse, and recovering is now the system
    /// under test's job.
    ///
    /// # Where recovery actually happens
    ///
    /// Between the cutoff and workload completion — the quiet tail — because
    /// that is the only window where the processes are still running and can
    /// re-elect, re-replicate, or re-sync. Give the workloads enough simulated
    /// time after the cutoff to converge, then assert on convergence from
    /// them.
    ///
    /// The settle phase that follows is *not* a recovery window: processes are
    /// aborted before it, so it only drains the events left in the scheduler
    /// before `check()` runs.
    #[must_use]
    pub fn chaos_duration(mut self, duration: Duration) -> Self {
        self.chaos_duration = Some(duration);
        self
    }

    /// Set the number of iterations to run.
    #[must_use]
    pub fn set_iterations(mut self, iterations: usize) -> Self {
        self.iteration_control = IterationControl::FixedCount(iterations);
        self
    }

    /// Set the virtual-time budget for each workload phase.
    ///
    /// If simulated time advances past this bound while one or more workloads
    /// are still running, the orchestrator requests shutdown. If the phase
    /// still cannot finish, it declares the seed deadlocked. Setup and final
    /// checks use the same guard, so a hung precondition or validation cannot
    /// keep a seed alive forever.
    ///
    /// This is a deterministic safety net for a *self-perpetuating timer*: a
    /// detached task (e.g. a reconnect / keepalive loop) that re-arms a
    /// [`crate::TimeProvider::sleep`] every tick keeps the event queue
    /// non-empty forever, so the no-progress deadlock detector never fires
    /// even though no workload-relevant progress is being made. The budget
    /// turns that silent hang into an actionable deadlock failure.
    ///
    /// The decision is a pure function of the simulated event schedule (no
    /// wall clock, no RNG), so it never perturbs replay determinism. The
    /// default (one simulated hour) is deliberately generous; raise it for
    /// legitimately long simulations.
    #[must_use]
    pub fn run_time_budget(mut self, budget: Duration) -> Self {
        self.run_time_budget = budget;
        self
    }

    /// Run until the system is saturated: every observed
    /// `assert_sometimes!` / `assert_reachable!` assertion has fired **and**
    /// code coverage has not grown for `plateau_seeds` consecutive seeds
    /// (capped at `max_iterations`). A simulation with no coverage assertion
    /// saturates on the plateau alone (with a warning).
    ///
    /// Uses real LLVM sancov code coverage when the binary is instrumented
    /// (built via `cargo xtask sim run`); otherwise falls back to assertion-slot
    /// coverage. Works with or without [`SimulationBuilder::enable_exploration`];
    /// no fork occurs unless exploration is explicitly enabled.
    /// e.g. `until_coverage_stable(10, 5000)`.
    #[must_use]
    pub fn until_coverage_stable(mut self, plateau_seeds: usize, max_iterations: usize) -> Self {
        self.iteration_control = IterationControl::UntilCoverageStable {
            plateau_seeds,
            max_iterations,
        };
        self
    }

    /// Set specific seeds for deterministic debugging and regression testing.
    #[must_use]
    pub fn set_debug_seeds(mut self, seeds: Vec<u64>) -> Self {
        self.seeds = seeds;
        self
    }

    /// Replay one explored timeline: run a single iteration with `seed` and
    /// install the recipe's RNG breakpoints, reproducing the exact timeline
    /// an exploration bug recipe describes (`BugRecipe { seed, recipe }`).
    #[must_use]
    pub fn replay_timeline(mut self, seed: u64, recipe: Vec<(u64, u64)>) -> Self {
        self.seeds = vec![seed];
        self.iteration_control = IterationControl::FixedCount(1);
        self.replay_recipe = Some(recipe);
        self
    }

    /// Enable chaos surfaces and choose how each is sampled per seed.
    ///
    /// Each [`Chaos`] entry turns on one surface (network / storage / attrition)
    /// in a given [`ChaosMode`] — `Random` (full surface every seed) or `Swarm`
    /// (a per-seed random *subset* of sub-families, the rest fully off). A surface
    /// not listed stays off. Later entries override earlier ones for the same
    /// surface, except attrition: every [`Chaos::Attrition`] entry adds one
    /// independent injector, so a campaign with several process groups can
    /// give each its own reboot regime, victim filter, and `max_dead` budget
    /// (a filtered injector spends its budget on its own pool only). Every
    /// [`Chaos::Outage`] entry likewise adds one correlated-outage injector.
    /// Attrition and outages, like every fault injector, run only inside the
    /// chaos window and therefore require [`Self::chaos_duration`].
    ///
    /// `Swarm` mode defeats passive suppression: when every fault is always
    /// slightly on (`Random`) families crowd each other out and the extreme
    /// single-family configs that surface bugs almost never occur. Subset
    /// decisions are draws on the simulation stream, seeded for the iteration,
    /// taken in a fixed order before the world is built, so the same seed
    /// rebuilds the same subset on every run and replay.
    ///
    /// The workload operation-alphabet swarm is a separate, test-driver concern —
    /// see [`swarm_operations`](Self::swarm_operations).
    ///
    /// # Example
    ///
    /// ```ignore
    /// builder.enable_chaos([
    ///     Chaos::Network(ChaosMode::Swarm),
    ///     Chaos::Storage(ChaosMode::Swarm),
    /// ]);
    ///
    /// // One reboot regime per process group: acceptors lose at most one
    /// // node at a time, matchmakers are hammered independently.
    /// builder.enable_chaos([
    ///     Chaos::Attrition {
    ///         config: Attrition {
    ///             max_dead: 1,
    ///             victims: AttritionVictims::group("acceptor"),
    ///             ..acceptor_regime
    ///         },
    ///         mode: ChaosMode::Swarm,
    ///     },
    ///     Chaos::Attrition {
    ///         config: Attrition {
    ///             max_dead: 3,
    ///             victims: AttritionVictims::group("matchmaker"),
    ///             ..matchmaker_regime
    ///         },
    ///         mode: ChaosMode::Random,
    ///     },
    /// ]);
    /// ```
    #[must_use]
    pub fn enable_chaos(mut self, surfaces: impl IntoIterator<Item = Chaos>) -> Self {
        for surface in surfaces {
            match surface {
                Chaos::Network(mode) => self.network_chaos = Some(mode),
                Chaos::Storage(mode) => self.storage_chaos = Some(mode),
                Chaos::Attrition { config, mode } => self.attritions.push((config, mode)),
                Chaos::Outage { config, mode } => self.outages.push((config, mode)),
                Chaos::BuggifyKnobs => self.buggify_knobs = true,
            }
        }
        self
    }

    /// Enable per-seed swarm testing of the workload operation alphabet.
    ///
    /// When enabled, each seed exposes a random *subset* of each workload's
    /// operation alphabet via [`swarm_op_enabled`](crate::swarm_op_enabled), so
    /// bugs reachable only when whole operation groups are suppressed become
    /// reachable across seeds. Decisions come from a dedicated per-seed stream and
    /// are reproducible. Independent of [`enable_chaos`](Self::enable_chaos).
    #[must_use]
    pub fn swarm_operations(mut self) -> Self {
        self.swarm_operations = true;
        self
    }

    /// Run every seed twice and fail it if the two runs differ — madsim's
    /// `Runtime::check_determinism`, as a canary on the simulation stream.
    ///
    /// The first run records a 64-bit fingerprint after every draw on the
    /// simulation stream: a probe of the generator's state after the draw
    /// (taken on a clone, so checking consumes no randomness and a checked run
    /// draws exactly what an unchecked one draws) mixed with the logical
    /// clock. The second run, same seed, compares each draw's fingerprint
    /// against the record, and afterwards the whole record must have been
    /// consumed. Because task scheduling, `select!` offsets, swarm masks and
    /// every fault coin are draws on that one stream, any uncontrolled
    /// difference — a `HashMap` iterated in random order, wall-clock time, a
    /// static that survives a run, an OS RNG — changes what gets drawn or when,
    /// and the seed fails with the always-assertion
    /// `"determinism canary: replay matched the recorded draw sequence"`
    /// naming the first diverging draw (or the early exit).
    ///
    /// A canary, not a trace: it says *that* the two runs diverged and at
    /// which draw, not why. Every seed costs two runs, and the replay's
    /// assertion evaluations are counted in the report beside the first run's.
    /// Like exploration it requires factory workloads (`workload_factory`,
    /// `workloads`): an instance workload would carry its state into the
    /// second run and diverge by construction.
    #[must_use]
    pub fn check_determinism(mut self) -> Self {
        self.check_determinism = true;
        self
    }

    /// Enable frontier-based multiverse exploration.
    ///
    /// When enabled, globally new assertion outcomes create replay recipes.
    /// Bounded worker processes replay those timelines and continue them with
    /// different deterministic seeds. Set `config.workers` to zero for
    /// sequential, fork-free exploration. Requires the `exploration` feature.
    ///
    /// Exploration requires factory-created workloads: instance workloads are
    /// rejected because the runner cannot reconstruct them with fresh state
    /// for every continuation timeline. Fault injectors are always
    /// factory-created ([`Self::fault_factory`] and the built-in [`Chaos`]
    /// surfaces), so a fresh injector is built for every root and explored
    /// timeline (both in-process `workers: 0` and forked-worker exploration).
    ///
    /// # Panics
    ///
    /// Panics when the configuration contains a zero exploration bound. If
    /// exploration is combined with instance workloads, [`Self::run`] returns
    /// [`SimulationError::InvalidConfiguration`] instead, because those values
    /// cannot be reconstructed for each continuation timeline. Use
    /// [`Self::workload_factory`] or [`Self::workloads`] instead.
    #[cfg(feature = "exploration")]
    #[must_use]
    pub fn enable_exploration(
        mut self,
        config: crate::chaos::exploration_glue::ExplorationConfig,
    ) -> Self {
        if let Err(error) = config.validate() {
            panic!("invalid exploration configuration: {error}");
        }
        self.exploration_config = Some(config);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use moonpool_core::RandomProvider;

    use crate::SimulationResult;
    use crate::runner::context::SimContext;
    use crate::runner::report::SimulationReport;

    struct BasicWorkload;

    #[async_trait]
    impl Workload for BasicWorkload {
        fn name(&self) -> &'static str {
            "test_workload"
        }

        async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
            Ok(())
        }
    }

    #[cfg(feature = "exploration")]
    fn test_exploration_config() -> crate::ExplorationConfig {
        crate::ExplorationConfig {
            workers: 0,
            max_runs_per_seed: 1,
            branching_factor: 1,
            max_frontier: 1,
            max_recipe_len: 1,
        }
    }

    /// The configuration error a builder's `run()` refused with.
    fn configuration_error(result: Result<SimulationReport, SimulationError>) -> String {
        match result {
            Err(SimulationError::InvalidConfiguration(message)) => message,
            other => panic!("expected InvalidConfiguration, got {other:?}"),
        }
    }

    #[test]
    fn attrition_without_chaos_duration_is_refused() {
        let result = SimulationBuilder::new()
            .workload(BasicWorkload)
            .enable_chaos([
                Chaos::Network(ChaosMode::Swarm),
                Chaos::Attrition {
                    config: Attrition {
                        max_dead: 1,
                        prob_graceful: 0.0,
                        prob_crash: 1.0,
                        prob_wipe: 0.0,
                        recovery_delay_ms: None,
                        grace_period_ms: None,
                        scope: crate::AttritionScope::PerProcess,
                        victims: crate::AttritionVictims::Any,
                    },
                    mode: ChaosMode::Random,
                },
            ])
            .set_iterations(1)
            .run();
        let message = configuration_error(result);
        assert!(
            message.starts_with(
                "1 Chaos::Attrition regime(s) registered without SimulationBuilder::chaos_duration"
            ),
            "{message}"
        );
    }

    #[test]
    fn fault_factory_without_chaos_duration_is_refused() {
        struct Idle;
        #[async_trait]
        impl FaultInjector for Idle {
            fn name(&self) -> &'static str {
                "idle"
            }
            async fn inject(&mut self, _ctx: &crate::FaultContext) -> SimulationResult<()> {
                Ok(())
            }
        }
        let result = SimulationBuilder::new()
            .workload(BasicWorkload)
            .fault_factory(|| Box::new(Idle))
            .set_iterations(1)
            .run();
        let message = configuration_error(result);
        assert!(
            message.starts_with("1 fault_factory injector(s) registered without"),
            "{message}"
        );
    }

    #[test]
    fn test_simulation_builder_basic() {
        let report = SimulationBuilder::new()
            .workload(BasicWorkload)
            .set_iterations(3)
            .set_debug_seeds(vec![1, 2, 3])
            .run()
            .expect("simulation configuration is valid");

        assert_eq!(report.iterations, 3);
        assert_eq!(report.successful_runs, 3);
        assert_eq!(report.failed_runs, 0);
        assert!((report.success_rate() - 100.0).abs() < f64::EPSILON);
        assert_eq!(report.seeds_used, vec![1, 2, 3]);
    }

    /// The world a Swarm + `BuggifyKnobs` storage campaign builds for
    /// `seed`, under `mask`, and the draws building it consumed.
    fn sampled_storage_world(
        mask: Option<crate::StorageFaultMask>,
        seed: u64,
    ) -> (crate::sim::SimWorld, u64) {
        crate::sim::reset_sim_rng();
        crate::sim::set_sim_seed(seed);
        let mut builder = SimulationBuilder::new()
            .enable_chaos([Chaos::Storage(ChaosMode::Swarm), Chaos::BuggifyKnobs]);
        if let Some(mask) = mask {
            builder = builder.storage_fault_mask(mask);
        }
        let sim = builder.build_sim_for_iteration(seed);
        (sim, crate::sim::rng_call_count())
    }

    /// Drive `future` and the world together until it finishes.
    async fn drive_storage<F: std::future::Future>(
        sim: &mut crate::sim::SimWorld,
        future: F,
    ) -> F::Output {
        futures::pin_mut!(future);
        std::future::poll_fn(|context| match future.as_mut().poll(context) {
            std::task::Poll::Ready(output) => std::task::Poll::Ready(output),
            std::task::Poll::Pending if sim.has_pending_events() => {
                sim.step();
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        })
        .await
    }

    /// moonpool#292's acceptance: masking a storage family after Swarm and
    /// `BuggifyKnobs` sampling keeps it out of every seed — its
    /// probabilities are zero in the world, and a write load records none
    /// of it — and consumes no draw, so each seed samples exactly what it
    /// samples unmasked. (The load also masks the disk-failure family: a
    /// failed disk leaves every later operation pending.)
    #[test]
    fn a_storage_fault_mask_keeps_its_family_out_and_draws_nothing() {
        use crate::{StorageFault, StorageFaultKind, StorageFaultMask};
        use moonpool_core::{OpenOptions, StorageFile, StorageProvider};

        let mask = StorageFaultMask::all()
            .without(StorageFault::PhantomWrite)
            .without(StorageFault::DiskFailure);
        let mut phantoms_drawn = 0;
        let mut writes = 0;
        for seed in 0..24 {
            let (mut masked, masked_draws) = sampled_storage_world(Some(mask), seed);
            let (full, full_draws) = sampled_storage_world(None, seed);
            assert_eq!(masked_draws, full_draws, "seed {seed}: the mask drew");
            let (phantom, failure) = masked.with_storage_config(|config| {
                (
                    config.phantom_write_probability,
                    config.disk_failure_probability,
                )
            });
            assert_eq!(
                (phantom.to_bits(), failure.to_bits()),
                (0, 0),
                "seed {seed}"
            );
            phantoms_drawn += usize::from(
                full.with_storage_config(|config| config.phantom_write_probability > 0.0),
            );

            let provider = masked.storage_provider(std::net::IpAddr::from([10, 0, 1, 1]));
            let block = vec![0xA5u8; 4096];
            let written = crate::executor::Executor::new(seed).block_on(async {
                drive_storage(&mut masked, async move {
                    let file = provider
                        .open("mask.dat", OpenOptions::create_write().read(true))
                        .await?;
                    let mut written = 0;
                    for at in 0..400u64 {
                        // A fault is an outcome here, not a failure.
                        written += usize::from(file.write_at(at * 4096, &block).await.is_ok());
                        if at % 8 == 7 {
                            let _ = file.sync_data().await;
                        }
                    }
                    Ok::<_, std::io::Error>(written)
                })
                .await
            });
            writes += written.expect("open the file");
            let records = masked.take_storage_fault_records();
            assert!(
                records
                    .iter()
                    .all(|record| record.kind != StorageFaultKind::PhantomWrite),
                "seed {seed}: a masked family fired: {records:?}"
            );
        }
        assert!(phantoms_drawn > 0, "no seed drew phantom writes to mask");
        assert!(writes > 0);
    }

    fn sampled_network_config(
        mode: ChaosMode,
        mask: crate::NetworkFaultMask,
        seed: u64,
    ) -> (crate::NetworkConfiguration, u64) {
        crate::sim::reset_sim_rng();
        crate::sim::set_sim_seed(seed);
        let sim = SimulationBuilder::new()
            .enable_chaos([Chaos::Network(mode)])
            .network_fault_mask(mask)
            .build_sim_for_iteration(seed);
        let config = sim.with_network_config(Clone::clone);
        let draws_consumed = crate::sim::rng_call_count();
        (config, draws_consumed)
    }

    #[test]
    fn accept_backlog_builder_override_reaches_world_without_changing_send_window() {
        let builder = SimulationBuilder::new().accept_backlog_capacity(3);
        let default_window = crate::NetworkConfiguration::default().tcp_send_window_bytes;
        let sim = builder.build_sim_for_iteration(20_260_915);
        sim.with_network_config(|config| {
            assert_eq!(config.accept_backlog_capacity, 3);
            assert_eq!(config.tcp_send_window_bytes, default_window);
        });
    }

    #[test]
    fn network_fault_mask_disables_only_bit_flips() {
        let seed = 174;
        let (baseline, _) =
            sampled_network_config(ChaosMode::Random, crate::NetworkFaultMask::all(), seed);
        let (masked, _) = sampled_network_config(
            ChaosMode::Random,
            crate::NetworkFaultMask::all().without(crate::NetworkFault::BitFlip),
            seed,
        );

        assert!(baseline.chaos.bit_flip_probability > 0.0);
        let mut expected = baseline;
        expected.chaos.bit_flip_probability = 0.0;
        assert_eq!(masked, expected, "the mask changed another fault family");
        assert!(masked.chaos.partial_read_max_bytes > 0);
        assert!(masked.chaos.partial_write_max_bytes > 0);
    }

    #[test]
    fn network_fault_mask_consumes_no_draws() {
        let (seed, baseline, baseline_draws) = (0..1000_u64)
            .find_map(|seed| {
                let (config, draws) =
                    sampled_network_config(ChaosMode::Swarm, crate::NetworkFaultMask::all(), seed);
                (config.chaos.bit_flip_probability > 0.0).then_some((seed, config, draws))
            })
            .expect("expected Swarm to select bit flips within 1000 seeds");
        let (masked, masked_draws) = sampled_network_config(
            ChaosMode::Swarm,
            crate::NetworkFaultMask::all().without(crate::NetworkFault::BitFlip),
            seed,
        );

        let mut expected = baseline;
        expected.chaos.bit_flip_probability = 0.0;
        assert_eq!(masked, expected);
        assert_eq!(masked_draws, baseline_draws);
    }

    #[cfg(feature = "exploration")]
    #[test]
    fn exploration_accepts_factory_workloads_and_builtin_chaos() {
        let builder = SimulationBuilder::new()
            .workload_factory(|| Box::new(BasicWorkload))
            .enable_chaos([Chaos::Network(ChaosMode::Random)])
            .enable_exploration(test_exploration_config());

        builder
            .validate_rerun_lifecycle()
            .expect("factory workloads can be rebuilt for every timeline");
    }

    #[cfg(feature = "exploration")]
    #[test]
    #[should_panic(expected = "max_frontier")]
    fn exploration_rejects_zero_config_bound() {
        let mut config = test_exploration_config();
        config.max_frontier = 0;

        let _builder = SimulationBuilder::new().enable_exploration(config);
    }

    #[cfg(feature = "exploration")]
    #[test]
    fn exploration_rejects_instance_workloads() {
        let result = SimulationBuilder::new()
            .workload(BasicWorkload)
            .enable_exploration(test_exploration_config())
            .run();
        let message = configuration_error(result);
        assert!(
            message
                .starts_with("exploration runs a seed more than once and requires fresh workloads"),
            "{message}"
        );
    }

    struct FailingWorkload;

    #[async_trait]
    impl Workload for FailingWorkload {
        fn name(&self) -> &'static str {
            "failing_workload"
        }

        async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
            // Deterministic: fail if first random number is even
            let random_num: u32 = ctx.random().random_range(0..100);
            if random_num.is_multiple_of(2) {
                return Err(crate::SimulationError::InvalidState(
                    "Test failure".to_string(),
                ));
            }
            Ok(())
        }
    }

    #[test]
    fn test_simulation_builder_with_failures() {
        // Pinned seeds: without them, iteration seeds derive from the wall
        // clock and this test demands both outcomes across 10 fair coin
        // flips, a ~0.2% spontaneous failure rate. Each seed's outcome is a
        // pure function of the seed, so pinning makes it deterministic.
        let report = SimulationBuilder::new()
            .workload(FailingWorkload)
            .set_debug_seeds((1..=10).collect())
            .set_iterations(10)
            .run()
            .expect("simulation configuration is valid");

        assert_eq!(report.iterations, 10);
        assert_eq!(
            report.successful_runs + report.failed_runs,
            10,
            "all iterations should be accounted for"
        );
        assert!(
            report.failed_runs > 0,
            "expected at least one failure across 10 seeds"
        );
        assert!(
            report.successful_runs > 0,
            "expected at least one success across 10 seeds"
        );
    }
}
