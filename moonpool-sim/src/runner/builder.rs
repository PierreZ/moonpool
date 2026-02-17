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

/// Internal storage for workload entries in the builder.
enum WorkloadEntry {
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
    iteration_control: IterationControl,
    entries: Vec<WorkloadEntry>,
    seeds: Vec<u64>,
    use_random_config: bool,
    invariants: Vec<Box<dyn Invariant>>,
    fault_injectors: Vec<Box<dyn FaultInjector>>,
    phase_config: Option<PhaseConfig>,
    exploration_config: Option<moonpool_explorer::ExplorationConfig>,
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
        }
    }

    /// Add a single workload instance to the simulation.
    ///
    /// The instance is reused across iterations (the `run()` method is called
    /// each iteration on the same struct).
    pub fn workload(mut self, w: impl Workload) -> Self {
        self.entries
            .push(WorkloadEntry::Instance(Some(Box::new(w))));
        self
    }

    /// Add multiple workload instances from a factory.
    ///
    /// The factory receives an instance index (0-based) and must return a fresh
    /// workload. Instances are created each iteration and dropped afterward.
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

    /// Resolve all entries into a flat workload list for one iteration.
    ///
    /// Returns `(workloads, return_map)` where `return_map[i] = Some(entry_idx)`
    /// means `workloads[i]` should be returned to `entries[entry_idx]` after the iteration.
    fn resolve_entries(&mut self) -> (Vec<Box<dyn Workload>>, Vec<Option<usize>>) {
        let mut workloads = Vec::new();
        let mut return_map = Vec::new();

        for (entry_idx, entry) in self.entries.iter_mut().enumerate() {
            match entry {
                WorkloadEntry::Instance(opt) => {
                    if let Some(w) = opt.take() {
                        return_map.push(Some(entry_idx));
                        workloads.push(w);
                    }
                }
                WorkloadEntry::Factory { count, factory } => {
                    let n = count.resolve();
                    for i in 0..n {
                        return_map.push(None);
                        workloads.push(factory(i));
                    }
                }
            }
        }

        (workloads, return_map)
    }

    /// Return instance-based workloads to their entry slots after an iteration.
    fn return_entries(
        &mut self,
        workloads: Vec<Box<dyn Workload>>,
        return_map: Vec<Option<usize>>,
    ) {
        for (w, slot) in workloads.into_iter().zip(return_map) {
            if let Some(entry_idx) = slot
                && let WorkloadEntry::Instance(opt) = &mut self.entries[entry_idx]
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
                exploration: None,
            };
        }

        // Initialize iteration state
        let mut iteration_manager =
            IterationManager::new(self.iteration_control.clone(), self.seeds.clone());
        let mut metrics_collector = MetricsCollector::new();

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

            // Prepare clean state for this iteration
            crate::sim::reset_sim_rng();
            crate::sim::set_sim_seed(seed);

            // Initialize buggify system for this iteration
            // Use moderate probabilities: 50% activation rate, 25% firing rate
            crate::chaos::buggify_init(0.5, 0.25);

            // Resolve workload entries into concrete instances for this iteration
            // (WorkloadCount::Random uses the sim RNG, already seeded above)
            let (workloads, return_map) = self.resolve_entries();

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

            let start_time = Instant::now();

            // Move fault injectors to orchestrator, get them back after
            let fault_injectors = std::mem::take(&mut self.fault_injectors);

            // Execute workloads using orchestrator
            let orchestration_result = WorkloadOrchestrator::orchestrate_workloads(
                workloads,
                fault_injectors,
                &self.invariants,
                &workload_info,
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
                    metrics_collector.record_iteration(seed, wall_time, &all_results, sim_metrics);
                }
                Err((faulty_seeds_from_deadlock, failed_count)) => {
                    // Handle deadlock case - merge with existing state and return early
                    metrics_collector.add_faulty_seeds(faulty_seeds_from_deadlock);
                    metrics_collector.add_failed_runs(failed_count);

                    // Create early exit report
                    let assertion_results = crate::chaos::get_assertion_results();
                    let assertion_violations = crate::chaos::validate_assertion_contracts();
                    crate::chaos::buggify_reset();

                    return metrics_collector.generate_report(
                        iteration_count,
                        iteration_manager.seeds_used().to_vec(),
                        assertion_results,
                        assertion_violations,
                        None,
                    );
                }
            }

            // Reset buggify state after each iteration to ensure clean state
            crate::chaos::buggify_reset();
        }

        // End of main iteration loop

        // Gather exploration stats before cleanup
        let exploration_report = if self.exploration_config.is_some() {
            let stats = moonpool_explorer::get_exploration_stats();
            let bug_recipe = moonpool_explorer::get_bug_recipe();
            moonpool_explorer::cleanup();
            stats.map(|s| super::report::ExplorationReport {
                total_timelines: s.total_timelines,
                fork_points: s.fork_points,
                bugs_found: s.bug_found,
                bug_recipe,
                energy_remaining: s.global_energy,
                realloc_pool_remaining: s.realloc_pool_remaining,
            })
        } else {
            None
        };

        // Print exploration summary (always visible, no tracing subscriber needed)
        if let Some(ref exp) = exploration_report {
            eprintln!(
                "\n--- Exploration ---\n  timelines: {}  |  fork points: {}  |  bugs: {}  |  energy left: {}",
                exp.total_timelines, exp.fork_points, exp.bugs_found, exp.energy_remaining
            );
            if let Some(ref recipe) = exp.bug_recipe {
                eprintln!(
                    "  bug recipe: {}",
                    moonpool_explorer::format_timeline(recipe)
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

        // Collect assertion results and validate them
        let assertion_results = crate::chaos::get_assertion_results();
        let assertion_violations = crate::chaos::validate_assertion_contracts();

        // Clean up assertion table if exploration didn't already do it
        if self.exploration_config.is_none() {
            moonpool_explorer::cleanup_assertions();
        }

        // Final buggify reset to ensure no impact on subsequent code
        crate::chaos::buggify_reset();

        metrics_collector.generate_report(
            iteration_count,
            iteration_manager.seeds_used().to_vec(),
            assertion_results,
            assertion_violations,
            exploration_report,
        )
    }
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
