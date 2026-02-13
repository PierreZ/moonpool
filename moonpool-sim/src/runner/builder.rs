//! Simulation builder pattern for configuring and running experiments.
//!
//! This module provides the main SimulationBuilder type for setting up
//! and executing simulation experiments.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::instrument;

use super::context::CancellationToken;
use super::fault_injector::PhaseConfig;
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
    /// Run until all sometimes_assert! assertions have been reached (with a safety limit)
    UntilAllSometimesReached(usize),
}

/// Builder pattern for configuring and running simulation experiments.
///
/// # Example
///
/// ```ignore
/// SimulationBuilder::new()
///     .workload_fn("client", |ctx| async move { Ok(()) })
///     .workload_fn("server", |ctx| async move { Ok(()) })
///     .invariant_fn("conservation", |state, _time| { /* check */ })
///     .set_iterations(100)
///     .random_network()
///     .run()
///     .await;
/// ```
pub struct SimulationBuilder {
    iteration_control: IterationControl,
    workloads: Vec<Box<dyn super::workload::Workload>>,
    invariants: Vec<Box<dyn crate::chaos::Invariant>>,
    fault_injectors: Vec<Box<dyn super::fault_injector::FaultInjector>>,
    phase_config: Option<PhaseConfig>,
    seeds: Vec<u64>,
    next_ip: u32,
    use_random_config: bool,
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
            workloads: Vec::new(),
            invariants: Vec::new(),
            fault_injectors: Vec::new(),
            phase_config: None,
            seeds: Vec::new(),
            next_ip: 1,
            use_random_config: false,
            exploration_config: None,
        }
    }

    /// Add a workload implementing the `Workload` trait.
    pub fn workload(mut self, w: impl super::workload::Workload + 'static) -> Self {
        self.workloads.push(Box::new(w));
        self
    }

    /// Add a workload from a name and async closure.
    pub fn workload_fn<F, Fut>(self, name: &str, run_fn: F) -> Self
    where
        F: Fn(&super::context::SimContext) -> Fut + 'static,
        Fut: std::future::Future<Output = Result<(), crate::SimulationError>> + 'static,
    {
        self.workload(super::workload::workload_fn(name, run_fn))
    }

    /// Add an invariant implementing the `Invariant` trait.
    pub fn invariant(mut self, inv: impl crate::chaos::Invariant + 'static) -> Self {
        self.invariants.push(Box::new(inv));
        self
    }

    /// Add an invariant from a name and closure.
    pub fn invariant_fn<F>(mut self, name: &str, check: F) -> Self
    where
        F: Fn(&crate::chaos::StateHandle, u64) + 'static,
    {
        self.invariants
            .push(crate::chaos::invariant_fn(name, check));
        self
    }

    /// Add a fault injector.
    pub fn fault(mut self, fi: impl super::fault_injector::FaultInjector + 'static) -> Self {
        self.fault_injectors.push(Box::new(fi));
        self
    }

    /// Set phase configuration for chaos/liveness phases.
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

    /// Run until all sometimes assertions have been reached.
    pub fn until_all_sometimes_reached(mut self, safety_limit: usize) -> Self {
        self.iteration_control = IterationControl::UntilAllSometimesReached(safety_limit);
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
    pub fn enable_exploration(mut self, config: moonpool_explorer::ExplorationConfig) -> Self {
        self.exploration_config = Some(config);
        self
    }

    /// Run the simulation and generate a report.
    #[instrument(skip_all)]
    pub async fn run(self) -> SimulationReport {
        if self.workloads.is_empty() {
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
            crate::chaos::buggify_init(0.5, 0.25);

            // Create fresh NetworkConfiguration for this iteration
            let network_config = if self.use_random_config {
                crate::NetworkConfiguration::random_for_seed()
            } else {
                crate::NetworkConfiguration::default()
            };

            // Create shared SimWorld for this iteration
            let sim = crate::sim::SimWorld::new_with_network_config_and_seed(network_config, seed);

            let start_time = Instant::now();

            // Create shutdown token for this iteration
            let shutdown = CancellationToken::new();

            // Assign IPs to workloads
            let mut workload_ips: Vec<String> = Vec::new();
            for i in 0..self.workloads.len() {
                workload_ips.push(format!("10.0.0.{}", self.next_ip as usize + i));
            }

            // Execute workloads using new orchestrator
            let orchestration_result = WorkloadOrchestrator::orchestrate(
                &self.workloads,
                &self.invariants,
                &self.fault_injectors,
                self.phase_config.as_ref(),
                &workload_ips,
                seed,
                sim,
                shutdown,
                iteration_count,
            )
            .await;

            let (all_results, sim_metrics) = match orchestration_result {
                Ok((results, metrics)) => (results, metrics),
                Err((faulty_seeds_from_deadlock, failed_count)) => {
                    metrics_collector.add_faulty_seeds(faulty_seeds_from_deadlock);
                    metrics_collector.add_failed_runs(failed_count);

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
            };

            let wall_time = start_time.elapsed();

            metrics_collector.record_iteration(seed, wall_time, all_results, sim_metrics);
            crate::chaos::buggify_reset();
        }

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

        // Log summary
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

        let assertion_results = crate::chaos::get_assertion_results();
        let assertion_violations = crate::chaos::validate_assertion_contracts();
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
