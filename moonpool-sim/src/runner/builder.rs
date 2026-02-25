//! Simulation builder pattern for configuring and running experiments.
//!
//! This module provides the main SimulationBuilder type for setting up
//! and executing simulation experiments.

use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};
use tracing::instrument;

use crate::chaos::invariant_trait::Invariant;
use crate::runner::fault_injector::{FaultInjector, PhaseConfig};
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
            let fault_injectors = std::mem::take(&mut self.fault_injectors);

            // Execute workloads using orchestrator
            let orchestration_result = WorkloadOrchestrator::orchestrate_workloads(
                workloads,
                fault_injectors,
                &self.invariants,
                &workload_info,
                &client_info,
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
                    );
                }
            }

            // Accumulate exploration stats across seeds (before reset)
            if self.exploration_config.is_some() {
                if let Some(stats) = moonpool_explorer::get_exploration_stats() {
                    total_exploration_timelines += stats.total_timelines;
                    total_exploration_fork_points += stats.fork_points;
                    total_exploration_bugs += stats.bug_found;
                }
                if let Some(recipe) = moonpool_explorer::get_bug_recipe() {
                    bug_recipes.push(super::report::BugRecipe { seed, recipe });
                }
            }

            // Reset buggify state after each iteration to ensure clean state
            crate::chaos::buggify_reset();
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

        // Print exploration summary (always visible, no tracing subscriber needed)
        if let Some(ref exp) = exploration_report {
            eprintln!(
                "\n--- Exploration ---\n  timelines: {}  |  fork points: {}  |  bugs: {}  |  energy left: {}  |  coverage: {}/{} ({:.1}%)",
                exp.total_timelines,
                exp.fork_points,
                exp.bugs_found,
                exp.energy_remaining,
                exp.coverage_bits,
                exp.coverage_total,
                if exp.coverage_total > 0 {
                    (exp.coverage_bits as f64 / exp.coverage_total as f64) * 100.0
                } else {
                    0.0
                }
            );
            for br in &exp.bug_recipes {
                eprintln!(
                    "  bug recipe: {}",
                    moonpool_explorer::format_timeline(&br.recipe)
                );
            }
        }

        // Log summary of all seeds used
        let iteration_count = iteration_manager.current_iteration();
        let (successful_runs, failed_runs) = metrics_collector.current_stats();
        tracing::info!(
            "Simulation completed: {}/{} iterations successful",
            successful_runs,
            iteration_count
        );
        tracing::info!("Seeds used: {:?}", iteration_manager.seeds_used());
        if failed_runs > 0 {
            tracing::warn!(
                "{} iterations failed - check logs above for failing seeds",
                failed_runs
            );
        }

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
