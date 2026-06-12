//! Simulation builder pattern for configuring and running experiments.
//!
//! This module provides the main `SimulationBuilder` type for setting up
//! and executing simulation experiments.

use std::collections::HashMap;
use std::ops::{Range, RangeInclusive};
use std::time::Duration;

use super::wall_clock::Instant;
use tracing::instrument;

use crate::SimulationError;
use crate::observability::{Invariant, SimulationLayer, SimulationLayerHandle, TraceQuery};
use crate::runner::fault_injector::FaultInjector;
use crate::runner::process::{Attrition, Process};
use crate::runner::tags::TagDistribution;
use crate::runner::workload::Workload;

use super::orchestrator::{
    GenerateReportInputs, IterationManager, MetricsCollector, OrchestrateInputs, OrchestrateOutput,
    WorkloadOrchestrator,
};

/// Client identity information for a single workload instance.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkloadClientInfo {
    /// The resolved client ID for this instance.
    pub(crate) client_id: usize,
    /// Total number of workload instances sharing this builder entry.
    pub(crate) client_count: usize,
}

/// Inputs to `run_orchestrator_blocking`.
struct RunOrchestratorInputs<'a> {
    seed: u64,
    iteration_count: usize,
    workloads: Vec<Box<dyn Workload>>,
    workload_info: Vec<(String, String)>,
    client_info: Vec<WorkloadClientInfo>,
    process_config: Option<super::orchestrator::ProcessConfig<'a>>,
    sim: crate::sim::SimWorld,
    fault_injectors: Vec<Box<dyn FaultInjector>>,
    chaos_duration: Option<Duration>,
    obs_handle: SimulationLayerHandle,
    run_time_budget: Duration,
}

/// Outcome of an orchestration attempt.
type OrchestrationOutcome = Result<OrchestrateOutput, (Vec<u64>, usize)>;

/// Per-run accumulators passed into the final-report builder.
struct FinalReportInputs {
    converged: bool,
    #[cfg(feature = "exploration")]
    total_exploration_timelines: u64,
    #[cfg(feature = "exploration")]
    total_exploration_fork_points: u64,
    #[cfg(feature = "exploration")]
    total_exploration_bugs: u64,
    #[cfg(feature = "exploration")]
    bug_recipes: Vec<super::report::BugRecipe>,
    #[cfg(feature = "exploration")]
    per_seed_timelines: Vec<u64>,
}

/// Aggregated state passed into the convergence / plateau check helper.
struct ConvergenceState<'a> {
    iteration_control: &'a IterationControl,
    iteration_count: usize,
    reached_sometimes: &'a std::collections::HashSet<String>,
    all_sometimes_count: usize,
    prev_coverage_bits: &'a mut u32,
    plateau_count: &'a mut usize,
    prev_reached_count: &'a mut usize,
    already_converged: bool,
}

impl RunState {
    /// Initialise per-run accumulators from the builder's configuration.
    fn new(builder: &SimulationBuilder) -> Self {
        let iteration_manager =
            IterationManager::new(builder.iteration_control.clone(), builder.seeds.clone());
        let progress_milestone = iteration_manager
            .max_iterations()
            .map(|max| std::cmp::max(max / 10, 1));
        Self {
            iteration_manager,
            metrics_collector: MetricsCollector::new(),
            progress_milestone,
            pending_return_map: Vec::new(),
            #[cfg(feature = "exploration")]
            total_exploration_timelines: 0,
            #[cfg(feature = "exploration")]
            total_exploration_fork_points: 0,
            #[cfg(feature = "exploration")]
            total_exploration_bugs: 0,
            #[cfg(feature = "exploration")]
            bug_recipes: Vec::new(),
            #[cfg(feature = "exploration")]
            per_seed_timelines: Vec::new(),
            reached_sometimes: std::collections::HashSet::new(),
            prev_coverage_bits: 0,
            converged: false,
            plateau_count: 0,
            prev_reached_count: 0,
        }
    }
}

/// Accumulated mutable state threaded through [`SimulationBuilder::run`].
struct RunState {
    iteration_manager: IterationManager,
    metrics_collector: MetricsCollector,
    /// Iteration interval at which progress is logged (`None` for unbounded runs).
    progress_milestone: Option<usize>,
    /// Map for routing iteration-resolved workloads back to their entry slots,
    /// stashed between [`SimulationBuilder::run_orchestrator_for_iteration`]
    /// and [`SimulationBuilder::handle_orchestration_result`].
    pending_return_map: Vec<Option<usize>>,
    // Exploration accumulators (only populated/read with the `exploration` feature).
    #[cfg(feature = "exploration")]
    total_exploration_timelines: u64,
    #[cfg(feature = "exploration")]
    total_exploration_fork_points: u64,
    #[cfg(feature = "exploration")]
    total_exploration_bugs: u64,
    #[cfg(feature = "exploration")]
    bug_recipes: Vec<super::report::BugRecipe>,
    #[cfg(feature = "exploration")]
    per_seed_timelines: Vec<u64>,
    // Convergence + plateau tracking.
    reached_sometimes: std::collections::HashSet<String>,
    prev_coverage_bits: u32,
    converged: bool,
    plateau_count: usize,
    prev_reached_count: usize,
}

/// Resolved workload entries for a single iteration.
struct ResolvedEntries {
    workloads: Vec<Box<dyn Workload>>,
    /// `return_map[i] = Some(entry_idx)` means `workloads[i]` should be
    /// returned to `entries[entry_idx]` after the iteration.
    return_map: Vec<Option<usize>>,
    /// Client identity info parallel to `workloads`.
    client_info: Vec<WorkloadClientInfo>,
}
use super::report::{SimulationMetrics, SimulationReport};

/// Configuration for how many iterations a simulation should run.
///
/// Provides flexible control over simulation execution duration and completion criteria.
#[derive(Debug, Clone)]
pub enum IterationControl {
    /// Run a fixed number of iterations with specific seeds
    FixedCount(usize),
    /// Run for a specific duration of wall-clock time
    TimeLimit(Duration),
    /// Stop when all `assert_sometimes!` have been reached AND a new seed
    /// produces no new code coverage. Requires exploration to be enabled.
    /// The `max_iterations` field is a safety cap.
    UntilConverged {
        /// Maximum number of seeds before stopping regardless.
        max_iterations: usize,
    },
    /// Stop after `plateau_seeds` consecutive seeds have added no new
    /// `assert_sometimes!` / `assert_reachable!` coverage. Works with or
    /// without [`SimulationBuilder::enable_exploration`]: the underlying
    /// assertion table is initialised unconditionally.
    CoveragePlateau {
        /// Number of consecutive seeds without coverage growth required to stop.
        plateau_seeds: usize,
        /// If true, additionally require every observed sometimes/reachable
        /// assertion to have fired at least once before stopping.
        require_all_sometimes: bool,
        /// Maximum number of seeds before stopping regardless (safety cap).
        max_iterations: usize,
    },
}

/// How many instances of a workload to spawn per iteration.
///
/// Use `Fixed` for deterministic topologies or `Random` for chaos testing
/// with varying cluster sizes.
///
/// # Examples
///
/// ```ignore
/// // Always 3 replicas
/// WorkloadCount::Fixed(3)
///
/// // 1 to 5 replicas, randomized per iteration
/// WorkloadCount::Random(1..6)
/// ```
#[derive(Debug, Clone)]
pub enum WorkloadCount {
    /// Spawn exactly N instances every iteration.
    Fixed(usize),
    /// Spawn a random number of instances in `[start..end)` per iteration,
    /// using the simulation RNG (deterministic per seed).
    Random(Range<usize>),
}

impl WorkloadCount {
    /// Resolve the count for the current iteration.
    /// For `Random`, uses the sim RNG which must already be seeded.
    fn resolve(&self) -> usize {
        match self {
            WorkloadCount::Fixed(n) => *n,
            WorkloadCount::Random(range) => crate::sim::sim_random_range(range.clone()),
        }
    }
}

/// Strategy for assigning client IDs to workload instances.
///
/// Inspired by `FoundationDB`'s `WorkloadContext.clientId`, but more
/// programmable. The resolved client ID is available via
/// [`SimContext::client_id()`](super::context::SimContext::client_id).
///
/// # Examples
///
/// ```ignore
/// // FDB-style sequential: IDs 0, 1, 2
/// ClientId::Fixed(0)
///
/// // Sequential starting from 10: IDs 10, 11, 12
/// ClientId::Fixed(10)
///
/// // Random IDs in [100..200) per instance
/// ClientId::RandomRange(100..200)
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ClientId {
    /// Sequential IDs starting from `base`: instance 0 gets `base`,
    /// instance 1 gets `base + 1`, and so on.
    Fixed(usize),
    /// Random ID drawn from `[start..end)` per instance,
    /// using the simulation RNG (deterministic per seed).
    /// IDs are not guaranteed unique across instances.
    RandomRange(Range<usize>),
}

impl Default for ClientId {
    fn default() -> Self {
        Self::Fixed(0)
    }
}

impl ClientId {
    /// Resolve a client ID for the given instance index.
    fn resolve(&self, index: usize) -> usize {
        match self {
            ClientId::Fixed(base) => base + index,
            ClientId::RandomRange(range) => crate::sim::sim_random_range(range.clone()),
        }
    }
}

/// How many process instances to spawn per iteration.
///
/// Use `Fixed` for deterministic topologies or `Range` for chaos testing
/// with varying cluster sizes.
///
/// # Examples
///
/// ```ignore
/// // Always 3 server processes
/// ProcessCount::Fixed(3)
///
/// // 3 to 7 server processes, randomized per iteration
/// ProcessCount::Range(3..=7)
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessCount {
    /// Spawn exactly N process instances every iteration.
    Fixed(usize),
    /// Spawn a random number in `[start..=end]` per iteration,
    /// using the simulation RNG (deterministic per seed).
    Range(RangeInclusive<usize>),
}

impl ProcessCount {
    /// Resolve the count for the current iteration.
    fn resolve(&self) -> usize {
        match self {
            ProcessCount::Fixed(n) => *n,
            ProcessCount::Range(range) => {
                let start = *range.start();
                let end = *range.end() + 1; // RangeInclusive -> exclusive for sim_random_range
                if start >= end {
                    return start;
                }
                crate::sim::sim_random_range(start..end)
            }
        }
    }
}

impl From<usize> for ProcessCount {
    fn from(n: usize) -> Self {
        ProcessCount::Fixed(n)
    }
}

impl From<RangeInclusive<usize>> for ProcessCount {
    fn from(range: RangeInclusive<usize>) -> Self {
        ProcessCount::Range(range)
    }
}

/// Internal storage for a process entry in the builder.
pub(crate) struct ProcessEntry {
    pub(crate) count: ProcessCount,
    pub(crate) factory: Box<dyn Fn() -> Box<dyn Process>>,
    pub(crate) tags: TagDistribution,
    pub(crate) name: String,
}

/// Internal storage for workload entries in the builder.
enum WorkloadEntry {
    /// Single instance, reused across iterations (from `.workload()`).
    Instance(Option<Box<dyn Workload>>, ClientId),
    /// Factory-based, fresh instances per iteration (from `.workloads()`).
    Factory {
        count: WorkloadCount,
        client_id: ClientId,
        factory: Box<dyn Fn(usize) -> Box<dyn Workload>>,
    },
}

/// Builder pattern for configuring and running simulation experiments.
pub struct SimulationBuilder {
    iteration_control: IterationControl,
    entries: Vec<WorkloadEntry>,
    process_entry: Option<ProcessEntry>,
    attrition: Option<Attrition>,
    seeds: Vec<u64>,
    use_random_config: bool,
    invariants: Vec<Box<dyn Invariant + Send>>,
    fault_injectors: Vec<Box<dyn FaultInjector>>,
    chaos_duration: Option<Duration>,
    exploration_config: Option<crate::chaos::exploration_glue::ExplorationConfig>,
    before_iteration_hooks: Vec<Box<dyn FnMut()>>,
    seed_warning_timeout: Option<Duration>,
    run_time_budget: Duration,
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
            iteration_control: IterationControl::FixedCount(1),
            entries: Vec::new(),
            process_entry: None,
            attrition: None,
            seeds: Vec::new(),
            use_random_config: false,
            invariants: Vec::new(),
            fault_injectors: Vec::new(),
            chaos_duration: None,
            exploration_config: None,
            before_iteration_hooks: Vec::new(),
            seed_warning_timeout: None,
            run_time_budget: super::orchestrator::DEFAULT_RUN_TIME_BUDGET,
        }
    }

    /// Add a single workload instance to the simulation.
    ///
    /// The instance is reused across iterations (the `run()` method is called
    /// each iteration on the same struct). Gets `client_id = 0`, `client_count = 1`.
    #[must_use]
    pub fn workload(mut self, w: impl Workload) -> Self {
        self.entries.push(WorkloadEntry::Instance(
            Some(Box::new(w)),
            ClientId::default(),
        ));
        self
    }

    /// Add server processes to the simulation.
    ///
    /// Processes represent the **system under test** — they can be killed and
    /// restarted (rebooted). A fresh instance is created from the factory on
    /// every boot.
    ///
    /// The `count` parameter accepts either a fixed `usize` or a
    /// `RangeInclusive<usize>` for seeded random count per iteration.
    ///
    /// Only one `.processes()` call is supported per builder. Subsequent calls
    /// overwrite the previous one.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Fixed 3 server processes
    /// builder.processes(3, || Box::new(MyNode::new()))
    ///
    /// // 3 to 7 processes, randomized per iteration
    /// builder.processes(3..=7, || Box::new(MyNode::new()))
    /// ```
    #[must_use]
    pub fn processes(
        mut self,
        count: impl Into<ProcessCount>,
        factory: impl Fn() -> Box<dyn Process> + 'static,
    ) -> Self {
        let sample = factory();
        let name = sample.name().to_string();
        drop(sample);
        self.process_entry = Some(ProcessEntry {
            count: count.into(),
            factory: Box::new(factory),
            tags: TagDistribution::new(),
            name,
        });
        self
    }

    /// Attach tag distribution to the last `.processes()` call.
    ///
    /// Tags are distributed round-robin across process instances. Each tag
    /// dimension is distributed independently.
    ///
    /// # Errors
    ///
    /// Returns `SimulationError::InvalidState` if called without a preceding
    /// `.processes()` call.
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
        let entry = self.process_entry.as_mut().ok_or_else(|| {
            SimulationError::InvalidState("tags() must be called after processes()".into())
        })?;
        for (key, values) in dimensions {
            entry.tags.add(key, values);
        }
        Ok(self)
    }

    /// Set built-in attrition for automatic process reboots during chaos phase.
    ///
    /// Attrition randomly kills and restarts server processes. It respects
    /// `max_dead` to limit the number of simultaneously dead processes.
    ///
    /// **Requires** [`.chaos_duration()`](Self::chaos_duration) — attrition injectors
    /// only run during the chaos phase. Without a chaos duration, the injector
    /// will not be spawned.
    ///
    /// For custom fault injection, use `.fault()` with a [`FaultInjector`] instead.
    #[must_use]
    pub fn attrition(mut self, config: Attrition) -> Self {
        self.attrition = Some(config);
        self
    }

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
            client_id: ClientId::default(),
            factory: Box::new(factory),
        });
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

    /// Add a fault injector to run during the chaos phase.
    #[must_use]
    pub fn fault(mut self, f: impl FaultInjector) -> Self {
        self.fault_injectors.push(Box::new(f));
        self
    }

    /// Set the chaos phase duration.
    ///
    /// When set, fault injectors run concurrently with workloads for this
    /// duration. After it elapses, faults stop and the system continues
    /// until all workloads complete. A settle phase then drains remaining
    /// events before checks run.
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

    /// Set the wall-clock time threshold for warning about slow seeds.
    ///
    /// When a seed takes longer than this duration, a `tracing::warn!` is emitted.
    /// If not set, no slow-seed warnings are produced.
    #[must_use]
    pub fn seed_warning_timeout(mut self, timeout: Duration) -> Self {
        self.seed_warning_timeout = Some(timeout);
        self
    }

    /// Set the virtual-time budget for a single run phase.
    ///
    /// If simulated time advances past this bound while one or more workloads
    /// are still running, the orchestrator first triggers a graceful shutdown
    /// and — if simulated time keeps climbing by another full budget while
    /// workloads remain — declares the run deadlocked.
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

    /// Run until exploration has converged: all `assert_sometimes!` assertions
    /// have been reached and no new coverage was found on the last seed.
    ///
    /// Requires `.enable_exploration()` to be configured (and the `exploration`
    /// feature). `max_iterations` is a safety cap to prevent infinite loops.
    #[cfg(feature = "exploration")]
    #[must_use]
    pub fn until_converged(mut self, max_iterations: usize) -> Self {
        self.iteration_control = IterationControl::UntilConverged { max_iterations };
        self
    }

    /// Run until `assert_sometimes!` / `assert_reachable!` coverage has not
    /// grown for `plateau_seeds` consecutive seeds.
    ///
    /// Works with or without [`SimulationBuilder::enable_exploration`].
    /// `max_iterations` is a safety cap to prevent infinite loops when the
    /// plateau is never reached.
    #[must_use]
    pub fn until_coverage_plateau(mut self, plateau_seeds: usize, max_iterations: usize) -> Self {
        self.iteration_control = IterationControl::CoveragePlateau {
            plateau_seeds,
            require_all_sometimes: false,
            max_iterations,
        };
        self
    }

    /// Like [`Self::until_coverage_plateau`] but also lets the caller require
    /// every observed sometimes/reachable assertion to have fired before the
    /// plateau is allowed to terminate the run.
    #[must_use]
    pub fn until_coverage_plateau_with(
        mut self,
        plateau_seeds: usize,
        require_all_sometimes: bool,
        max_iterations: usize,
    ) -> Self {
        self.iteration_control = IterationControl::CoveragePlateau {
            plateau_seeds,
            require_all_sometimes,
            max_iterations,
        };
        self
    }

    /// Register a callback invoked at the start of each simulation iteration.
    ///
    /// Use this to reset shared state (directories, membership, stores) that
    /// lives outside the builder and is shared via `Rc` across iterations.
    #[must_use]
    pub fn before_iteration(mut self, f: impl FnMut() + 'static) -> Self {
        self.before_iteration_hooks.push(Box::new(f));
        self
    }

    /// Set specific seeds for deterministic debugging and regression testing.
    #[must_use]
    pub fn set_debug_seeds(mut self, seeds: Vec<u64>) -> Self {
        self.seeds = seeds;
        self
    }

    /// Enable randomized network configuration for chaos testing.
    #[must_use]
    pub fn random_network(mut self) -> Self {
        self.use_random_config = true;
        self
    }

    /// Enable fork-based multiverse exploration.
    ///
    /// When enabled, the simulation will fork child processes at assertion
    /// discovery points to explore alternate timelines with different seeds.
    /// Requires the `exploration` feature.
    #[cfg(feature = "exploration")]
    #[must_use]
    pub fn enable_exploration(
        mut self,
        config: crate::chaos::exploration_glue::ExplorationConfig,
    ) -> Self {
        self.exploration_config = Some(config);
        self
    }

    /// Resolve all entries into a flat workload list for one iteration.
    fn resolve_entries(&mut self) -> ResolvedEntries {
        let mut workloads = Vec::new();
        let mut return_map = Vec::new();
        let mut client_info = Vec::new();

        for (entry_idx, entry) in self.entries.iter_mut().enumerate() {
            match entry {
                WorkloadEntry::Instance(opt, cid) => {
                    if let Some(w) = opt.take() {
                        return_map.push(Some(entry_idx));
                        client_info.push(WorkloadClientInfo {
                            client_id: cid.resolve(0),
                            client_count: 1,
                        });
                        workloads.push(w);
                    }
                }
                WorkloadEntry::Factory {
                    count,
                    client_id,
                    factory,
                } => {
                    let n = count.resolve();
                    for i in 0..n {
                        return_map.push(None);
                        client_info.push(WorkloadClientInfo {
                            client_id: client_id.resolve(i),
                            client_count: n,
                        });
                        workloads.push(factory(i));
                    }
                }
            }
        }

        ResolvedEntries {
            workloads,
            return_map,
            client_info,
        }
    }

    /// Return instance-based workloads to their entry slots after an iteration.
    fn return_entries(
        &mut self,
        workloads: Vec<Box<dyn Workload>>,
        return_map: Vec<Option<usize>>,
    ) {
        for (w, slot) in workloads.into_iter().zip(return_map) {
            if let Some(entry_idx) = slot
                && let WorkloadEntry::Instance(opt, _) = &mut self.entries[entry_idx]
            {
                *opt = Some(w);
            }
            // Factory-created workloads are dropped
        }
    }

    /// Spin up a fresh tokio runtime, run the orchestrator on it, and
    /// return its outcome.
    fn run_orchestrator_blocking(inputs: RunOrchestratorInputs<'_>) -> OrchestrationOutcome {
        let RunOrchestratorInputs {
            seed,
            iteration_count,
            workloads,
            workload_info,
            client_info,
            process_config,
            sim,
            fault_injectors,
            chaos_duration,
            obs_handle,
            run_time_budget,
        } = inputs;
        let local_runtime = Self::build_local_runtime_for_seed(seed);
        local_runtime.block_on(async move {
            WorkloadOrchestrator::orchestrate_workloads(OrchestrateInputs {
                workloads,
                fault_injectors,
                obs: obs_handle,
                workload_info: &workload_info,
                client_info: &client_info,
                process_config,
                seed,
                sim,
                chaos_duration,
                iteration_count,
                run_time_budget,
            })
            .await
        })
    }

    /// Build a `SimWorld` for the iteration, picking a randomised or default
    /// network configuration.
    fn build_sim_for_iteration(use_random: bool, seed: u64) -> crate::sim::SimWorld {
        let network_config = if use_random {
            crate::NetworkConfiguration::random_for_seed()
        } else {
            crate::NetworkConfiguration::default()
        };
        crate::sim::SimWorld::new_with_network_config_and_seed(network_config, seed)
    }

    /// Drain user-provided fault injectors and, when present, append the
    /// built-in attrition injector.
    fn collect_fault_injectors(
        user_injectors: &mut Vec<Box<dyn FaultInjector>>,
        attrition: Option<&Attrition>,
    ) -> Vec<Box<dyn FaultInjector>> {
        let mut fault_injectors = std::mem::take(user_injectors);
        if let Some(attrition) = attrition {
            fault_injectors.push(Box::new(
                crate::runner::fault_injector::AttritionInjector::new(attrition.clone()),
            ));
        }
        fault_injectors
    }

    /// Build an early-exit report on deadlock: snapshot the assertion
    /// state, reset buggify, and consume the metrics collector.
    fn build_early_exit_report(
        metrics_collector: MetricsCollector,
        iteration_count: usize,
        seeds_used: Vec<u64>,
    ) -> SimulationReport {
        let assertion_results = crate::chaos::assertion_results();
        let (assertion_violations, coverage_violations) =
            crate::chaos::validate_assertion_contracts();
        crate::chaos::buggify_reset();
        metrics_collector.generate_report(GenerateReportInputs {
            iteration_count,
            seeds_used,
            assertion_results,
            assertion_violations,
            coverage_violations,
            exploration: None,
            assertion_details: Vec::new(),
            bucket_summaries: Vec::new(),
            convergence_timeout: false,
        })
    }

    /// Check whether the user's convergence / plateau condition has been
    /// met this iteration. Returns the new `converged` flag.
    fn check_convergence_or_plateau(state: ConvergenceState<'_>) -> bool {
        let ConvergenceState {
            iteration_control,
            iteration_count,
            reached_sometimes,
            all_sometimes_count,
            prev_coverage_bits,
            plateau_count,
            prev_reached_count,
            already_converged,
        } = state;
        if already_converged {
            return true;
        }
        match iteration_control {
            IterationControl::UntilConverged { .. } => {
                if iteration_count < 2 {
                    return false;
                }
                let all_reached =
                    all_sometimes_count > 0 && reached_sometimes.len() >= all_sometimes_count;
                let current_bits = crate::chaos::exploration_glue::explored_coverage_bits();
                let no_new_coverage = current_bits == *prev_coverage_bits;
                tracing::warn!(
                    "convergence: seed={} reached={}/{} coverage={}->{} delta={}",
                    iteration_count,
                    reached_sometimes.len(),
                    all_sometimes_count,
                    *prev_coverage_bits,
                    current_bits,
                    current_bits.saturating_sub(*prev_coverage_bits),
                );
                if all_reached && no_new_coverage {
                    tracing::info!(
                        "Converged after {} seeds: all {} sometimes reached, no new coverage",
                        iteration_count,
                        all_sometimes_count
                    );
                    return true;
                }
                false
            }
            IterationControl::CoveragePlateau {
                plateau_seeds,
                require_all_sometimes,
                ..
            } => {
                let current_count = reached_sometimes.len();
                if iteration_count == 1 {
                    *prev_reached_count = current_count;
                } else if current_count == *prev_reached_count {
                    *plateau_count += 1;
                } else {
                    *plateau_count = 0;
                    *prev_reached_count = current_count;
                }
                let all_reached = !*require_all_sometimes
                    || (all_sometimes_count > 0 && reached_sometimes.len() >= all_sometimes_count);
                tracing::warn!(
                    "plateau: seed={} reached={}/{} consecutive_no_growth={}/{}",
                    iteration_count,
                    current_count,
                    all_sometimes_count,
                    *plateau_count,
                    plateau_seeds,
                );
                if *plateau_count >= *plateau_seeds && all_reached {
                    tracing::info!(
                        "Coverage plateau after {} seeds: {} consecutive without growth, {} sometimes reached",
                        iteration_count,
                        *plateau_count,
                        current_count
                    );
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Emit a `warn!` when an iteration exceeded the configured threshold.
    fn log_slow_seed(seed: u64, wall_time: Duration, threshold: Option<Duration>) {
        if let Some(threshold) = threshold
            && wall_time > threshold
        {
            tracing::warn!(
                seed,
                wall_time_ms = u64::try_from(wall_time.as_millis()).unwrap_or(u64::MAX),
                threshold_ms = u64::try_from(threshold.as_millis()).unwrap_or(u64::MAX),
                "seed took {:.2}s (threshold: {}s)",
                wall_time.as_secs_f64(),
                threshold.as_secs(),
            );
        }
    }

    /// Emit a milestone `info!` every `progress_milestone` iterations.
    fn log_progress_milestone(
        progress_milestone: Option<usize>,
        iteration_count: usize,
        max: usize,
    ) {
        if let Some(interval) = progress_milestone
            && iteration_count.is_multiple_of(interval)
        {
            let iteration_f64 = u32::try_from(iteration_count).map_or(f64::INFINITY, f64::from);
            let max_f64 = u32::try_from(max).map_or(f64::INFINITY, f64::from);
            let pct = (iteration_f64 / max_f64) * 100.0;
            tracing::info!(
                iteration = iteration_count,
                total = max,
                "[{}/{}] {:.0}% complete",
                iteration_count,
                max,
                pct,
            );
        }
    }

    /// Build a fresh single-threaded tokio runtime seeded for this iteration.
    /// When dropped, ALL tasks are killed — no orphan tasks leak between
    /// iterations.
    fn build_local_runtime_for_seed(seed: u64) -> tokio::runtime::Runtime {
        let mut seed_bytes = [0u8; 32];
        seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());
        let rng_seed = tokio::runtime::RngSeed::from_bytes(&seed_bytes);
        // No .enable_time(): the sim drives logical time via the event queue, not
        // tokio's timer wheel. enable_time() also constructs a std::time::Instant
        // at startup, which panics on wasm32-unknown-unknown.
        tokio::runtime::Builder::new_current_thread()
            .rng_seed(rng_seed)
            .build()
            .expect("per-iteration runtime")
    }

    /// Reset per-iteration state: capture buffers, RNG, buggify, and chaos.
    fn reset_per_iteration_state(seed: u64, obs_handle: &SimulationLayerHandle) {
        obs_handle.reset_for_seed();
        crate::sim::reset_sim_rng();
        crate::sim::set_sim_seed(seed);
        crate::chaos::reset_always_violations();
        // Use moderate probabilities: 50% activation rate, 25% firing rate.
        crate::chaos::buggify_init(0.5, 0.25);
    }

    /// Resolve a process entry into a `ProcessConfig` for the current
    /// iteration, sampling the count/tags from the sim RNG (already seeded).
    fn resolve_process_config(entry: &ProcessEntry) -> super::orchestrator::ProcessConfig<'_> {
        let count = entry.count.resolve();
        let mut registry = crate::runner::tags::TagRegistry::new();
        let mut ips = Vec::with_capacity(count);
        let mut info = Vec::with_capacity(count);
        let base_name = &entry.name;
        for i in 0..count {
            let ip = format!("10.0.1.{}", i + 1);
            let ip_addr: std::net::IpAddr = ip.parse().expect("valid process IP");
            let tags = entry.tags.resolve(i);
            registry.register(ip_addr, tags);
            ips.push(ip.clone());
            let name = if count == 1 {
                base_name.clone()
            } else {
                format!("{base_name}-{i}")
            };
            info.push((name, ip));
        }
        super::orchestrator::ProcessConfig {
            factory: &*entry.factory,
            info,
            ips,
            tag_registry: registry,
        }
    }

    /// Initialise the assertion region (heap, or `MAP_SHARED` + explorer), and
    /// activate exploration when a config is present.
    fn init_assertions_and_exploration(
        exploration_config: Option<&crate::chaos::exploration_glue::ExplorationConfig>,
    ) {
        crate::chaos::exploration_glue::init_assertion_region();
        let _ = exploration_config;
        #[cfg(feature = "exploration")]
        if let Some(config) = exploration_config {
            moonpool_explorer::set_rng_hooks(crate::sim::rng_call_count, |seed| {
                crate::sim::set_sim_seed(seed);
                crate::sim::reset_rng_call_count();
            });
            if let Err(e) = moonpool_explorer::init(config) {
                tracing::error!("Failed to initialize exploration: {}", e);
            }
        }
    }

    /// Build the final `ExplorationReport` from the running totals collected
    /// across iterations.
    #[cfg(feature = "exploration")]
    fn build_exploration_report(
        total_timelines: u64,
        total_fork_points: u64,
        total_bugs: u64,
        bug_recipes: Vec<super::report::BugRecipe>,
        converged: bool,
        per_seed_timelines: Vec<u64>,
    ) -> super::report::ExplorationReport {
        let final_stats = moonpool_explorer::exploration_stats();
        let coverage_bits = moonpool_explorer::explored_map_bits_set().unwrap_or(0);
        super::report::ExplorationReport {
            total_timelines,
            fork_points: total_fork_points,
            bugs_found: total_bugs,
            bug_recipes,
            energy_remaining: final_stats.as_ref().map_or(0, |s| s.global_energy),
            realloc_pool_remaining: final_stats.as_ref().map_or(0, |s| s.realloc_pool_remaining),
            coverage_bits,
            coverage_total: u32::try_from(moonpool_explorer::coverage::COVERAGE_MAP_SIZE * 8)
                .expect("coverage map size fits in u32"),
            sancov_edges_total: final_stats.as_ref().map_or(0, |s| s.sancov_edges_total),
            sancov_edges_covered: final_stats.as_ref().map_or(0, |s| s.sancov_edges_covered),
            converged,
            per_seed_timelines,
        }
    }

    /// Read the explorer's per-seed exploration stats and accumulate into
    /// the totals + per-seed timelines arrays. Captures any new bug recipe
    /// produced this seed.
    #[cfg(feature = "exploration")]
    fn accumulate_exploration_stats(
        seed: u64,
        per_seed_timelines: &mut Vec<u64>,
        total_timelines: &mut u64,
        total_fork_points: &mut u64,
        total_bugs: &mut u64,
        bug_recipes: &mut Vec<super::report::BugRecipe>,
    ) {
        if let Some(stats) = moonpool_explorer::exploration_stats() {
            per_seed_timelines.push(stats.total_timelines);
            *total_timelines += stats.total_timelines;
            *total_fork_points += stats.fork_points;
            *total_bugs += stats.bug_found;
        } else {
            per_seed_timelines.push(0);
        }
        if let Some(recipe) = moonpool_explorer::bug_recipe() {
            bug_recipes.push(super::report::BugRecipe { seed, recipe });
        }
    }

    /// Scan all assertion slots from shared memory: insert the messages
    /// of every "passed" Sometimes/Reachable slot into `reached`, log a
    /// warning for every still-unreached slot, and return the count of
    /// unique Sometimes/Reachable message strings observed.
    fn scan_assertion_slots(reached: &mut std::collections::HashSet<String>) -> usize {
        let slots = moonpool_assertions::assertion_read_all();
        for slot in &slots {
            if let Some(kind) = moonpool_assertions::AssertKind::from_u8(slot.kind)
                && matches!(
                    kind,
                    moonpool_assertions::AssertKind::Sometimes
                        | moonpool_assertions::AssertKind::Reachable
                )
            {
                if slot.pass_count > 0 {
                    reached.insert(slot.msg.clone());
                } else if !reached.contains(&slot.msg) {
                    tracing::warn!(
                        "UNREACHED slot: kind={:?} msg={:?} pass={} fail={}",
                        kind,
                        slot.msg,
                        slot.pass_count,
                        slot.fail_count
                    );
                }
            }
        }
        slots
            .iter()
            .filter(|s| {
                moonpool_assertions::AssertKind::from_u8(s.kind).is_some_and(|k| {
                    matches!(
                        k,
                        moonpool_assertions::AssertKind::Sometimes
                            | moonpool_assertions::AssertKind::Reachable
                    )
                })
            })
            .map(|s| s.msg.clone())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// Build the empty report returned when no workloads are registered.
    fn empty_report() -> SimulationReport {
        SimulationReport {
            iterations: 0,
            successful_runs: 0,
            failed_runs: 0,
            metrics: SimulationMetrics::default(),
            individual_metrics: Vec::new(),
            seeds_used: Vec::new(),
            seeds_failing: Vec::new(),
            assertion_results: HashMap::new(),
            assertion_violations: Vec::new(),
            coverage_violations: Vec::new(),
            exploration: None,
            assertion_details: Vec::new(),
            bucket_summaries: Vec::new(),
            convergence_timeout: false,
        }
    }

    #[instrument(skip_all)]
    /// Run the simulation and generate a report.
    ///
    /// Creates a fresh tokio `LocalRuntime` per iteration for full isolation —
    /// all tasks are killed when the runtime is dropped at iteration end.
    ///
    /// # Panics
    ///
    /// Panics if a simulation invariant fails or a workload panics.
    pub fn run(mut self) -> SimulationReport {
        if self.entries.is_empty() {
            return Self::empty_report();
        }

        // Install the observability layer once for the entire run. The guard
        // is dropped when run() returns, restoring the previous subscriber.
        // All registered invariants live on the layer handle.
        let layer = SimulationLayer::new();
        let (obs_handle, _obs_guard) = layer.install();
        for inv in self.invariants.drain(..) {
            obs_handle.register(inv);
        }

        Self::init_assertions_and_exploration(self.exploration_config.as_ref());

        assert!(
            !matches!(
                self.iteration_control,
                IterationControl::UntilConverged { .. }
            ) || self.exploration_config.is_some(),
            "IterationControl::UntilConverged requires enable_exploration() to be configured"
        );

        let mut state = RunState::new(&self);

        while state.iteration_manager.should_continue() {
            if let Some(report) = self.execute_iteration(&mut state, &obs_handle) {
                return report;
            }
            if state.converged {
                break;
            }
        }

        Self::build_final_report(
            state.metrics_collector,
            &state.iteration_manager,
            self.exploration_config.as_ref(),
            &self.iteration_control,
            &FinalReportInputs {
                converged: state.converged,
                #[cfg(feature = "exploration")]
                total_exploration_timelines: state.total_exploration_timelines,
                #[cfg(feature = "exploration")]
                total_exploration_fork_points: state.total_exploration_fork_points,
                #[cfg(feature = "exploration")]
                total_exploration_bugs: state.total_exploration_bugs,
                #[cfg(feature = "exploration")]
                bug_recipes: state.bug_recipes,
                #[cfg(feature = "exploration")]
                per_seed_timelines: state.per_seed_timelines,
            },
        )
    }

    /// Execute one iteration of the run loop. Returns `Some(report)` when the
    /// loop must terminate early (e.g. orchestrator deadlock).
    fn execute_iteration(
        &mut self,
        state: &mut RunState,
        obs_handle: &SimulationLayerHandle,
    ) -> Option<SimulationReport> {
        let seed = state.iteration_manager.next_iteration();
        let iteration_count = state.iteration_manager.current_iteration();

        self.prepare_iteration(state, obs_handle, seed, iteration_count);

        let (orchestration_result, start_time) =
            self.run_orchestrator_for_iteration(state, obs_handle, seed, iteration_count);

        if let Err(report) = self.handle_orchestration_result(
            state,
            orchestration_result,
            seed,
            iteration_count,
            start_time,
        ) {
            return Some(*report);
        }

        self.finish_iteration(state, seed, iteration_count);
        None
    }

    /// Run all per-iteration setup steps before the orchestrator starts:
    /// prepare-next-seed, user hooks, reset state, snapshot coverage.
    fn prepare_iteration(
        &mut self,
        state: &mut RunState,
        obs_handle: &SimulationLayerHandle,
        seed: u64,
        iteration_count: usize,
    ) {
        // Preserve assertion data across iterations so the final report
        // reflects all seeds, not just the last one. For exploration runs,
        // prepare_next_seed() also does a selective reset of coverage state.
        if iteration_count > 1 {
            #[cfg(feature = "exploration")]
            if let Some(ref config) = self.exploration_config {
                moonpool_explorer::prepare_next_seed(config.global_energy);
            }
            crate::chaos::assertions::skip_next_assertion_reset();
        }

        for hook in &mut self.before_iteration_hooks {
            hook();
        }

        Self::reset_per_iteration_state(seed, obs_handle);

        if matches!(
            self.iteration_control,
            IterationControl::UntilConverged { .. }
        ) {
            state.prev_coverage_bits = crate::chaos::exploration_glue::explored_coverage_bits();
        }
    }

    /// Resolve workload entries, build the per-iteration sim/fault-injectors,
    /// and drive the orchestrator. Stashes `return_map` in `state` for the
    /// subsequent result-handling step. Returns the orchestration outcome and
    /// the wall-clock start time of the orchestrator call (used for slow-seed
    /// logging).
    fn run_orchestrator_for_iteration(
        &mut self,
        state: &mut RunState,
        obs_handle: &SimulationLayerHandle,
        seed: u64,
        iteration_count: usize,
    ) -> (OrchestrationOutcome, Instant) {
        let ResolvedEntries {
            workloads,
            return_map,
            client_info,
        } = self.resolve_entries();
        state.pending_return_map = return_map;

        let workload_info: Vec<(String, String)> = workloads
            .iter()
            .enumerate()
            .map(|(i, w)| (w.name().to_string(), format!("10.0.0.{}", i + 1)))
            .collect();

        let process_config = self
            .process_entry
            .as_ref()
            .map(Self::resolve_process_config);

        let sim = Self::build_sim_for_iteration(self.use_random_config, seed);
        let start_time = Instant::now();
        let fault_injectors =
            Self::collect_fault_injectors(&mut self.fault_injectors, self.attrition.as_ref());
        let outcome = Self::run_orchestrator_blocking(RunOrchestratorInputs {
            seed,
            iteration_count,
            workloads,
            workload_info,
            client_info,
            process_config,
            sim,
            fault_injectors,
            chaos_duration: self.chaos_duration,
            obs_handle: obs_handle.clone(),
            run_time_budget: self.run_time_budget,
        });
        (outcome, start_time)
    }

    /// Process the orchestration outcome: route the success path back into
    /// state, or build an early-exit report on deadlock.
    fn handle_orchestration_result(
        &mut self,
        state: &mut RunState,
        result: OrchestrationOutcome,
        seed: u64,
        iteration_count: usize,
        start_time: Instant,
    ) -> Result<(), Box<SimulationReport>> {
        let max_iterations = state
            .iteration_manager
            .max_iterations()
            .unwrap_or(iteration_count);
        let seeds_used_snapshot = state.iteration_manager.seeds_used().to_vec();
        match result {
            Ok(OrchestrateOutput {
                workloads: returned_workloads,
                fault_injectors: returned_injectors,
                results: all_results,
                metrics: sim_metrics,
            }) => {
                let return_map = std::mem::take(&mut state.pending_return_map);
                self.return_entries(returned_workloads, return_map);
                self.fault_injectors = returned_injectors;
                let wall_time = start_time.elapsed();
                state.metrics_collector.record_iteration(
                    seed,
                    wall_time,
                    &all_results,
                    crate::chaos::has_always_violations(),
                    sim_metrics,
                );
                Self::log_slow_seed(seed, wall_time, self.seed_warning_timeout);
                Self::log_progress_milestone(
                    state.progress_milestone,
                    iteration_count,
                    max_iterations,
                );
                Ok(())
            }
            Err((faulty_seeds_from_deadlock, failed_count)) => {
                state
                    .metrics_collector
                    .add_faulty_seeds(faulty_seeds_from_deadlock);
                state.metrics_collector.add_failed_runs(failed_count);
                let metrics_collector =
                    std::mem::replace(&mut state.metrics_collector, MetricsCollector::new());
                Err(Box::new(Self::build_early_exit_report(
                    metrics_collector,
                    iteration_count,
                    seeds_used_snapshot,
                )))
            }
        }
    }

    /// Run all per-iteration cleanup steps after the orchestrator finished:
    /// accumulate exploration stats, run the convergence scan, reset buggify.
    fn finish_iteration(&self, state: &mut RunState, seed: u64, iteration_count: usize) {
        // `seed` is only consumed by the exploration stats accumulation below.
        #[cfg(not(feature = "exploration"))]
        let _ = seed;
        #[cfg(feature = "exploration")]
        if self.exploration_config.is_some() {
            Self::accumulate_exploration_stats(
                seed,
                &mut state.per_seed_timelines,
                &mut state.total_exploration_timelines,
                &mut state.total_exploration_fork_points,
                &mut state.total_exploration_bugs,
                &mut state.bug_recipes,
            );
        }

        let needs_assertion_scan = matches!(
            self.iteration_control,
            IterationControl::UntilConverged { .. } | IterationControl::CoveragePlateau { .. }
        );
        if needs_assertion_scan {
            let all_sometimes_count = Self::scan_assertion_slots(&mut state.reached_sometimes);
            state.converged = Self::check_convergence_or_plateau(ConvergenceState {
                iteration_control: &self.iteration_control,
                iteration_count,
                reached_sometimes: &state.reached_sometimes,
                all_sometimes_count,
                prev_coverage_bits: &mut state.prev_coverage_bits,
                plateau_count: &mut state.plateau_count,
                prev_reached_count: &mut state.prev_reached_count,
                already_converged: state.converged,
            });
        }

        crate::chaos::buggify_reset();
    }

    /// Drain shared-memory state, free it, then build the final report.
    fn build_final_report(
        metrics_collector: MetricsCollector,
        iteration_manager: &IterationManager,
        exploration_config: Option<&crate::chaos::exploration_glue::ExplorationConfig>,
        iteration_control: &IterationControl,
        inputs: &FinalReportInputs,
    ) -> SimulationReport {
        let converged = inputs.converged;

        // 1. Read exploration-specific data (freed by cleanup). Without the
        // `exploration` feature there is none — the report's `exploration` field
        // is simply `None`, keeping the public report shape identical. The two
        // accumulator Vecs are cloned once here (report time only).
        #[cfg(feature = "exploration")]
        let exploration_report = if exploration_config.is_some() {
            Some(Self::build_exploration_report(
                inputs.total_exploration_timelines,
                inputs.total_exploration_fork_points,
                inputs.total_exploration_bugs,
                inputs.bug_recipes.clone(),
                converged,
                inputs.per_seed_timelines.clone(),
            ))
        } else {
            None
        };
        #[cfg(not(feature = "exploration"))]
        let exploration_report: Option<super::report::ExplorationReport> = None;

        // 2. Read assertion + bucket data (freed by cleanup/cleanup_assertions).
        let assertion_results = crate::chaos::assertion_results();
        let (assertion_violations, coverage_violations) =
            crate::chaos::validate_assertion_contracts();
        let raw_assertion_slots = moonpool_assertions::assertion_read_all();
        let raw_each_buckets = moonpool_assertions::each_bucket_read_all();

        // 3. Now safe to free all shared memory. Under exploration `cleanup()`
        // frees the exploration regions (the assertion table persists, as before);
        // otherwise free the assertion region directly.
        let did_exploration_cleanup = {
            #[cfg(feature = "exploration")]
            {
                if exploration_config.is_some() {
                    moonpool_explorer::cleanup();
                    true
                } else {
                    false
                }
            }
            #[cfg(not(feature = "exploration"))]
            {
                let _ = exploration_config;
                false
            }
        };
        if !did_exploration_cleanup {
            crate::chaos::exploration_glue::cleanup_assertion_region();
        }

        let assertion_details = build_assertion_details(&raw_assertion_slots);
        let bucket_summaries = build_bucket_summaries(&raw_each_buckets);
        let iteration_count = iteration_manager.current_iteration();

        // Detect convergence timeout.
        let convergence_timeout = matches!(
            iteration_control,
            IterationControl::UntilConverged { .. } | IterationControl::CoveragePlateau { .. }
        ) && !converged;

        crate::chaos::buggify_reset();

        metrics_collector.generate_report(GenerateReportInputs {
            iteration_count,
            seeds_used: iteration_manager.seeds_used().to_vec(),
            assertion_results,
            assertion_violations,
            coverage_violations,
            exploration: exploration_report,
            assertion_details,
            bucket_summaries,
            convergence_timeout,
        })
    }
}

/// Build [`AssertionDetail`] vec from raw assertion slot snapshots.
fn build_assertion_details(
    slots: &[moonpool_assertions::AssertionSlotSnapshot],
) -> Vec<super::report::AssertionDetail> {
    use super::report::{AssertionDetail, AssertionStatus};
    use moonpool_assertions::AssertKind;

    slots
        .iter()
        .filter_map(|slot| {
            let kind = AssertKind::from_u8(slot.kind)?;
            let total = slot.pass_count.saturating_add(slot.fail_count);

            // Skip unvisited assertions
            if total == 0 && slot.frontier == 0 {
                return None;
            }

            let status = match kind {
                AssertKind::Always
                | AssertKind::AlwaysOrUnreachable
                | AssertKind::NumericAlways => {
                    if slot.fail_count > 0 {
                        AssertionStatus::Fail
                    } else {
                        AssertionStatus::Pass
                    }
                }
                AssertKind::Sometimes | AssertKind::NumericSometimes | AssertKind::Reachable => {
                    if slot.pass_count > 0 {
                        AssertionStatus::Pass
                    } else {
                        AssertionStatus::Miss
                    }
                }
                AssertKind::Unreachable => {
                    if slot.pass_count > 0 {
                        AssertionStatus::Fail
                    } else {
                        AssertionStatus::Pass
                    }
                }
                AssertKind::BooleanSometimesAll => {
                    if slot.frontier > 0 {
                        AssertionStatus::Pass
                    } else {
                        AssertionStatus::Miss
                    }
                }
            };

            Some(AssertionDetail {
                msg: slot.msg.clone(),
                kind,
                pass_count: slot.pass_count,
                fail_count: slot.fail_count,
                watermark: slot.watermark,
                frontier: slot.frontier,
                status,
            })
        })
        .collect()
}

/// Build [`BucketSiteSummary`] vec by grouping [`EachBucket`]s by site message.
fn build_bucket_summaries(
    buckets: &[moonpool_assertions::EachBucket],
) -> Vec<super::report::BucketSiteSummary> {
    use super::report::BucketSiteSummary;
    use std::collections::HashMap;

    let mut sites: HashMap<u32, BucketSiteSummary> = HashMap::new();

    for bucket in buckets {
        let entry = sites
            .entry(bucket.site_hash)
            .or_insert_with(|| BucketSiteSummary {
                msg: bucket.msg_str().to_string(),
                buckets_discovered: 0,
                total_hits: 0,
            });

        entry.buckets_discovered += 1;
        entry.total_hits += u64::from(bucket.pass_count);
    }

    let mut summaries: Vec<_> = sites.into_values().collect();
    summaries.sort_by_key(|s| std::cmp::Reverse(s.total_hits));
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use moonpool_core::RandomProvider;

    use crate::SimulationResult;
    use crate::runner::context::SimContext;

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

    #[test]
    fn test_simulation_builder_basic() {
        let report = SimulationBuilder::new()
            .workload(BasicWorkload)
            .set_iterations(3)
            .set_debug_seeds(vec![1, 2, 3])
            .run();

        assert_eq!(report.iterations, 3);
        assert_eq!(report.successful_runs, 3);
        assert_eq!(report.failed_runs, 0);
        assert!((report.success_rate() - 100.0).abs() < f64::EPSILON);
        assert_eq!(report.seeds_used, vec![1, 2, 3]);
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
        let report = SimulationBuilder::new()
            .workload(FailingWorkload)
            .set_iterations(10)
            .run();

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
