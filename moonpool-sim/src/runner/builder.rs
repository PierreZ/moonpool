//! Simulation builder pattern for configuring and running experiments.
//!
//! This module provides the main SimulationBuilder type for setting up
//! and executing simulation experiments.

use std::collections::HashMap;
use std::ops::{Range, RangeInclusive};
use std::time::{Duration, Instant};
use tracing::instrument;

use crate::chaos::invariant_trait::Invariant;
use crate::runner::fault_injector::{FaultInjector, PhaseConfig};
use crate::runner::process::{Attrition, Process};
use crate::runner::tags::TagDistribution;
use crate::runner::workload::Workload;

use super::orchestrator::{IterationManager, MetricsCollector, WorkloadOrchestrator};

/// Client identity information for a single workload instance.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkloadClientInfo {
    /// The resolved client ID for this instance.
    pub(crate) client_id: usize,
    /// Total number of workload instances sharing this builder entry.
    pub(crate) client_count: usize,
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
/// Inspired by FoundationDB's `WorkloadContext.clientId`, but more
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
    invariants: Vec<Box<dyn Invariant>>,
    fault_injectors: Vec<Box<dyn FaultInjector>>,
    phase_config: Option<PhaseConfig>,
    exploration_config: Option<moonpool_explorer::ExplorationConfig>,
    replay_recipe: Option<super::report::BugRecipe>,
}

impl Default for SimulationBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulationBuilder {
    /// Create a new empty simulation builder.
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
            phase_config: None,
            exploration_config: None,
            replay_recipe: None,
        }
    }

    /// Add a single workload instance to the simulation.
    ///
    /// The instance is reused across iterations (the `run()` method is called
    /// each iteration on the same struct). Gets `client_id = 0`, `client_count = 1`.
    pub fn workload(mut self, w: impl Workload) -> Self {
        self.entries.push(WorkloadEntry::Instance(
            Some(Box::new(w)),
            ClientId::default(),
        ));
        self
    }

    /// Add a single workload instance with a custom client ID strategy.
    ///
    /// Like [`workload()`](Self::workload), but the resolved client ID is
    /// available via [`SimContext::client_id()`](super::context::SimContext::client_id).
    pub fn workload_with_client_id(mut self, client_id: ClientId, w: impl Workload) -> Self {
        self.entries
            .push(WorkloadEntry::Instance(Some(Box::new(w)), client_id));
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
    /// # Panics
    ///
    /// Panics if called without a preceding `.processes()` call.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // 5 processes: dc cycles east/west/eu, rack cycles r1/r2
    /// builder.processes(5, || Box::new(MyNode::new()))
    ///     .tags(&[
    ///         ("dc", &["east", "west", "eu"]),
    ///         ("rack", &["r1", "r2"]),
    ///     ])
    /// ```
    pub fn tags(mut self, dimensions: &[(&str, &[&str])]) -> Self {
        let entry = self
            .process_entry
            .as_mut()
            .expect("tags() must be called after processes()");
        for (key, values) in dimensions {
            entry.tags.add(key, values);
        }
        self
    }

    /// Set built-in attrition for automatic process reboots during chaos phase.
    ///
    /// Attrition randomly kills and restarts server processes. It respects
    /// `max_dead` to limit the number of simultaneously dead processes.
    ///
    /// **Requires** [`.phases()`](Self::phases) — attrition injectors only run during
    /// the chaos phase. Without a phase config, the injector will not be spawned.
    ///
    /// For custom fault injection, use `.fault()` with a [`FaultInjector`] instead.
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

    /// Add multiple workload instances with a custom client ID strategy.
    ///
    /// Like [`workloads()`](Self::workloads), but client IDs are assigned
    /// according to the given [`ClientId`] strategy instead of sequential.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // 3 clients with random IDs in [100..200)
    /// builder.workloads_with_client_id(
    ///     WorkloadCount::Fixed(3),
    ///     ClientId::RandomRange(100..200),
    ///     |i| Box::new(ClientWorkload::new(i)),
    /// )
    /// ```
    pub fn workloads_with_client_id(
        mut self,
        count: WorkloadCount,
        client_id: ClientId,
        factory: impl Fn(usize) -> Box<dyn Workload> + 'static,
    ) -> Self {
        self.entries.push(WorkloadEntry::Factory {
            count,
            client_id,
            factory: Box::new(factory),
        });
        self
    }

    /// Add an invariant to be checked after every simulation event.
    pub fn invariant(mut self, i: impl Invariant) -> Self {
        self.invariants.push(Box::new(i));
        self
    }

    /// Add a closure-based invariant.
    pub fn invariant_fn(
        mut self,
        name: impl Into<String>,
        f: impl Fn(&crate::chaos::StateHandle, u64) + 'static,
    ) -> Self {
        self.invariants.push(crate::chaos::invariant_fn(name, f));
        self
    }

    /// Add a fault injector to run during the chaos phase.
    pub fn fault(mut self, f: impl FaultInjector) -> Self {
        self.fault_injectors.push(Box::new(f));
        self
    }

    /// Set two-phase chaos/recovery configuration.
    pub fn phases(mut self, config: PhaseConfig) -> Self {
        self.phase_config = Some(config);
        self
    }

    /// Set the number of iterations to run.
    pub fn set_iterations(mut self, iterations: usize) -> Self {
        self.iteration_control = IterationControl::FixedCount(iterations);
        self
    }

    /// Set the iteration control strategy.
    pub fn set_iteration_control(mut self, control: IterationControl) -> Self {
        self.iteration_control = control;
        self
    }

    /// Run for a specific wall-clock time duration.
    pub fn set_time_limit(mut self, duration: Duration) -> Self {
        self.iteration_control = IterationControl::TimeLimit(duration);
        self
    }

    /// Run until exploration has converged: all `assert_sometimes!` assertions
    /// have been reached and no new coverage was found on the last seed.
    ///
    /// Requires `.enable_exploration()` to be configured.
    /// `max_iterations` is a safety cap to prevent infinite loops.
    pub fn until_converged(mut self, max_iterations: usize) -> Self {
        self.iteration_control = IterationControl::UntilConverged { max_iterations };
        self
    }

    /// Set specific seeds for deterministic debugging and regression testing.
    pub fn set_debug_seeds(mut self, seeds: Vec<u64>) -> Self {
        self.seeds = seeds;
        self
    }

    /// Enable randomized network configuration for chaos testing.
    pub fn random_network(mut self) -> Self {
        self.use_random_config = true;
        self
    }

    /// Enable fork-based multiverse exploration.
    ///
    /// When enabled, the simulation will fork child processes at assertion
    /// discovery points to explore alternate timelines with different seeds.
    pub fn enable_exploration(mut self, config: moonpool_explorer::ExplorationConfig) -> Self {
        self.exploration_config = Some(config);
        self
    }

    /// Set a bug recipe for deterministic replay.
    ///
    /// The builder applies the recipe's RNG breakpoints after its own
    /// initialization, ensuring they survive internal resets.
    pub fn replay_recipe(mut self, recipe: super::report::BugRecipe) -> Self {
        self.replay_recipe = Some(recipe);
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

    #[instrument(skip_all)]
    /// Run the simulation and generate a report.
    pub async fn run(mut self) -> SimulationReport {
        if self.entries.is_empty() {
            return SimulationReport {
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
            };
        }

        // Initialize iteration state
        let mut iteration_manager =
            IterationManager::new(self.iteration_control.clone(), self.seeds.clone());
        let mut metrics_collector = MetricsCollector::new();

        // Accumulators for multi-seed exploration stats
        let mut total_exploration_timelines: u64 = 0;
        let mut total_exploration_fork_points: u64 = 0;
        let mut total_exploration_bugs: u64 = 0;
        let mut bug_recipes: Vec<super::report::BugRecipe> = Vec::new();
        let mut per_seed_timelines: Vec<u64> = Vec::new();

        // Convergence tracking (used only with UntilConverged)
        let mut reached_sometimes: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut prev_coverage_bits: u32 = 0;
        let mut converged = false;

        // Initialize assertion table (unconditional — works even without exploration)
        if let Err(e) = moonpool_explorer::init_assertions() {
            tracing::error!("Failed to initialize assertion table: {}", e);
        }

        // Initialize exploration if configured
        if let Some(ref config) = self.exploration_config {
            moonpool_explorer::set_rng_hooks(crate::sim::get_rng_call_count, |seed| {
                crate::sim::set_sim_seed(seed);
                crate::sim::reset_rng_call_count();
            });
            if let Err(e) = moonpool_explorer::init(config.clone()) {
                tracing::error!("Failed to initialize exploration: {}", e);
            }
        }

        // Validate UntilConverged requires exploration
        if matches!(
            self.iteration_control,
            IterationControl::UntilConverged { .. }
        ) && self.exploration_config.is_none()
        {
            panic!(
                "IterationControl::UntilConverged requires enable_exploration() to be configured"
            );
        }

        while iteration_manager.should_continue() {
            let seed = iteration_manager.next_iteration();
            let iteration_count = iteration_manager.current_iteration();

            // Preserve assertion data across iterations so the final report
            // reflects all seeds, not just the last one.  For exploration runs,
            // prepare_next_seed() also does a selective reset of coverage state.
            if iteration_count > 1 {
                if let Some(ref config) = self.exploration_config {
                    moonpool_explorer::prepare_next_seed(config.global_energy);
                }
                crate::chaos::assertions::skip_next_assertion_reset();
            }

            // Prepare clean state for this iteration
            crate::sim::reset_sim_rng();
            crate::sim::set_sim_seed(seed);
            crate::chaos::reset_always_violations();

            // Initialize buggify system for this iteration
            // Use moderate probabilities: 50% activation rate, 25% firing rate
            crate::chaos::buggify_init(0.5, 0.25);

            // Snapshot coverage before this seed for convergence detection
            if matches!(
                self.iteration_control,
                IterationControl::UntilConverged { .. }
            ) {
                prev_coverage_bits = moonpool_explorer::explored_map_bits_set().unwrap_or(0);
            }

            // Resolve workload entries into concrete instances for this iteration
            // (WorkloadCount::Random and ClientId::RandomRange use the sim RNG, already seeded above)
            let ResolvedEntries {
                workloads,
                return_map,
                client_info,
            } = self.resolve_entries();

            // Compute workload name/IP pairs from resolved workloads
            let workload_info: Vec<(String, String)> = workloads
                .iter()
                .enumerate()
                .map(|(i, w)| (w.name().to_string(), format!("10.0.0.{}", i + 1)))
                .collect();

            // Resolve process configuration (if any)
            let process_config = self.process_entry.as_ref().map(
                |entry| -> super::orchestrator::ProcessConfig<'_> {
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
                            format!("{}-{}", base_name, i)
                        };
                        info.push((name, ip));
                    }
                    super::orchestrator::ProcessConfig {
                        factory: &*entry.factory,
                        info,
                        ips,
                        tag_registry: registry,
                    }
                },
            );

            // Create fresh NetworkConfiguration for this iteration
            let network_config = if self.use_random_config {
                crate::NetworkConfiguration::random_for_seed()
            } else {
                crate::NetworkConfiguration::default()
            };

            // Create shared SimWorld for this iteration using fresh network config
            let sim = crate::sim::SimWorld::new_with_network_config_and_seed(network_config, seed);

            // Apply replay breakpoints after SimWorld creation (which resets RNG state)
            if let Some(ref br) = self.replay_recipe {
                crate::sim::set_rng_breakpoints(br.recipe.clone());
            }

            let start_time = Instant::now();

            // Move fault injectors to orchestrator, get them back after
            let mut fault_injectors = std::mem::take(&mut self.fault_injectors);

            // Add built-in attrition injector if configured
            if let Some(ref attrition) = self.attrition {
                fault_injectors.push(Box::new(
                    crate::runner::fault_injector::AttritionInjector::new(attrition.clone()),
                ));
            }

            // Execute workloads using orchestrator
            let orchestration_result = WorkloadOrchestrator::orchestrate_workloads(
                workloads,
                fault_injectors,
                &self.invariants,
                &workload_info,
                &client_info,
                process_config,
                seed,
                sim,
                self.phase_config.as_ref(),
                iteration_count,
            )
            .await;

            match orchestration_result {
                Ok((returned_workloads, returned_injectors, all_results, sim_metrics)) => {
                    // Return Instance workloads to their entry slots
                    self.return_entries(returned_workloads, return_map);
                    self.fault_injectors = returned_injectors;

                    let wall_time = start_time.elapsed();
                    let has_violations = crate::chaos::has_always_violations();

                    metrics_collector.record_iteration(
                        seed,
                        wall_time,
                        &all_results,
                        has_violations,
                        sim_metrics,
                    );
                }
                Err((faulty_seeds_from_deadlock, failed_count)) => {
                    // Handle deadlock case - merge with existing state and return early
                    metrics_collector.add_faulty_seeds(faulty_seeds_from_deadlock);
                    metrics_collector.add_failed_runs(failed_count);

                    // Create early exit report
                    let assertion_results = crate::chaos::get_assertion_results();
                    let (assertion_violations, coverage_violations) =
                        crate::chaos::validate_assertion_contracts();
                    crate::chaos::buggify_reset();

                    return metrics_collector.generate_report(
                        iteration_count,
                        iteration_manager.seeds_used().to_vec(),
                        assertion_results,
                        assertion_violations,
                        coverage_violations,
                        None,
                        Vec::new(),
                        Vec::new(),
                        false,
                    );
                }
            }

            // Accumulate exploration stats across seeds (before reset)
            if self.exploration_config.is_some() {
                if let Some(stats) = moonpool_explorer::get_exploration_stats() {
                    per_seed_timelines.push(stats.total_timelines);
                    total_exploration_timelines += stats.total_timelines;
                    total_exploration_fork_points += stats.fork_points;
                    total_exploration_bugs += stats.bug_found;
                } else {
                    per_seed_timelines.push(0);
                }
                if let Some(recipe) = moonpool_explorer::get_bug_recipe() {
                    bug_recipes.push(super::report::BugRecipe { seed, recipe });
                }
            }

            // Accumulate which Sometimes/Reachable assertions have been reached
            // (must read before next prepare_next_seed resets pass_count)
            if matches!(
                self.iteration_control,
                IterationControl::UntilConverged { .. }
            ) {
                let slots = moonpool_explorer::assertion_read_all();
                for slot in &slots {
                    if let Some(kind) = moonpool_explorer::AssertKind::from_u8(slot.kind)
                        && matches!(
                            kind,
                            moonpool_explorer::AssertKind::Sometimes
                                | moonpool_explorer::AssertKind::Reachable
                        )
                    {
                        if slot.pass_count > 0 {
                            reached_sometimes.insert(slot.msg.clone());
                        } else if !reached_sometimes.contains(&slot.msg) {
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

                // Check convergence from seed 2 onward (need baseline for coverage delta)
                if iteration_count >= 2 {
                    let all_sometimes_count = slots
                        .iter()
                        .filter(|s| {
                            moonpool_explorer::AssertKind::from_u8(s.kind)
                                .map(|k| {
                                    matches!(
                                        k,
                                        moonpool_explorer::AssertKind::Sometimes
                                            | moonpool_explorer::AssertKind::Reachable
                                    )
                                })
                                .unwrap_or(false)
                        })
                        .count();
                    let all_reached =
                        all_sometimes_count > 0 && reached_sometimes.len() >= all_sometimes_count;

                    let current_bits = moonpool_explorer::explored_map_bits_set().unwrap_or(0);
                    let no_new_coverage = current_bits == prev_coverage_bits;

                    tracing::warn!(
                        "convergence: seed={} reached={}/{} coverage={}->{} delta={}",
                        iteration_count,
                        reached_sometimes.len(),
                        all_sometimes_count,
                        prev_coverage_bits,
                        current_bits,
                        current_bits.saturating_sub(prev_coverage_bits),
                    );

                    if all_reached && no_new_coverage {
                        tracing::info!(
                            "Converged after {} seeds: all {} sometimes reached, no new coverage",
                            iteration_count,
                            all_sometimes_count
                        );
                        converged = true;
                    }
                }
            }

            // Reset buggify state after each iteration to ensure clean state
            crate::chaos::buggify_reset();

            if converged {
                break;
            }
        }

        // End of main iteration loop
        //
        // Data collection: read ALL shared memory BEFORE any cleanup.
        // cleanup() calls cleanup_assertions() which frees the assertion
        // table and each-bucket table.

        // 1. Read exploration-specific data (freed by cleanup)
        let exploration_report = if self.exploration_config.is_some() {
            let final_stats = moonpool_explorer::get_exploration_stats();
            // The per-iteration capture above should have caught all recipes.
            // No fallback needed since we capture after every iteration.
            let coverage_bits = moonpool_explorer::explored_map_bits_set().unwrap_or(0);

            Some(super::report::ExplorationReport {
                total_timelines: total_exploration_timelines,
                fork_points: total_exploration_fork_points,
                bugs_found: total_exploration_bugs,
                bug_recipes,
                energy_remaining: final_stats.as_ref().map(|s| s.global_energy).unwrap_or(0),
                realloc_pool_remaining: final_stats
                    .as_ref()
                    .map(|s| s.realloc_pool_remaining)
                    .unwrap_or(0),
                coverage_bits,
                coverage_total: (moonpool_explorer::coverage::COVERAGE_MAP_SIZE * 8) as u32,
                sancov_edges_total: final_stats
                    .as_ref()
                    .map(|s| s.sancov_edges_total)
                    .unwrap_or(0),
                sancov_edges_covered: final_stats
                    .as_ref()
                    .map(|s| s.sancov_edges_covered)
                    .unwrap_or(0),
                converged,
                per_seed_timelines,
            })
        } else {
            None
        };

        // 2. Read assertion + bucket data (freed by cleanup/cleanup_assertions)
        let assertion_results = crate::chaos::get_assertion_results();
        let (assertion_violations, coverage_violations) =
            crate::chaos::validate_assertion_contracts();
        let raw_assertion_slots = moonpool_explorer::assertion_read_all();
        let raw_each_buckets = moonpool_explorer::each_bucket_read_all();

        // 3. Now safe to free all shared memory
        if self.exploration_config.is_some() {
            moonpool_explorer::cleanup();
        } else {
            moonpool_explorer::cleanup_assertions();
        }

        // 4. Build rich assertion details from raw slot snapshots
        let assertion_details = build_assertion_details(&raw_assertion_slots);

        // 5. Build bucket summaries by grouping EachBuckets by site
        let bucket_summaries = build_bucket_summaries(&raw_each_buckets);

        let iteration_count = iteration_manager.current_iteration();

        // Detect convergence timeout: UntilConverged was used but we didn't converge
        let convergence_timeout = matches!(
            self.iteration_control,
            IterationControl::UntilConverged { .. }
        ) && !converged;

        // Final buggify reset to ensure no impact on subsequent code
        crate::chaos::buggify_reset();

        metrics_collector.generate_report(
            iteration_count,
            iteration_manager.seeds_used().to_vec(),
            assertion_results,
            assertion_violations,
            coverage_violations,
            exploration_report,
            assertion_details,
            bucket_summaries,
            convergence_timeout,
        )
    }
}

/// Build [`AssertionDetail`] vec from raw assertion slot snapshots.
fn build_assertion_details(
    slots: &[moonpool_explorer::AssertionSlotSnapshot],
) -> Vec<super::report::AssertionDetail> {
    use super::report::{AssertionDetail, AssertionStatus};
    use moonpool_explorer::AssertKind;

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
                AssertKind::Sometimes | AssertKind::NumericSometimes => {
                    if slot.pass_count > 0 {
                        AssertionStatus::Pass
                    } else {
                        AssertionStatus::Miss
                    }
                }
                AssertKind::Reachable => {
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
    buckets: &[moonpool_explorer::EachBucket],
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
        entry.total_hits += bucket.pass_count as u64;
    }

    let mut summaries: Vec<_> = sites.into_values().collect();
    summaries.sort_by(|a, b| b.total_hits.cmp(&a.total_hits));
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

    #[async_trait(?Send)]
    impl Workload for BasicWorkload {
        fn name(&self) -> &str {
            "test_workload"
        }

        async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
            Ok(())
        }
    }

    #[test]
    fn test_simulation_builder_basic() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async move {
            SimulationBuilder::new()
                .workload(BasicWorkload)
                .set_iterations(3)
                .set_debug_seeds(vec![1, 2, 3])
                .run()
                .await
        });

        assert_eq!(report.iterations, 3);
        assert_eq!(report.successful_runs, 3);
        assert_eq!(report.failed_runs, 0);
        assert_eq!(report.success_rate(), 100.0);
        assert_eq!(report.seeds_used, vec![1, 2, 3]);
    }

    struct FailingWorkload;

    #[async_trait(?Send)]
    impl Workload for FailingWorkload {
        fn name(&self) -> &str {
            "failing_workload"
        }

        async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
            // Deterministic: fail if first random number is even
            let random_num: u32 = ctx.random().random_range(0..100);
            if random_num % 2 == 0 {
                return Err(crate::SimulationError::InvalidState(
                    "Test failure".to_string(),
                ));
            }
            Ok(())
        }
    }

    #[test]
    fn test_simulation_builder_with_failures() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async move {
            SimulationBuilder::new()
                .workload(FailingWorkload)
                .set_iterations(10)
                .run()
                .await
        });

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
