//! Simulation builder pattern for configuring and running experiments.
//!
//! This module provides the main SimulationBuilder type for setting up
//! and executing simulation experiments.

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};
use tracing::instrument;

use crate::SimulationResult;
use crate::chaos::invariant_trait::Invariant;
use crate::runner::fault_injector::{FaultInjector, PhaseConfig};
use crate::runner::workload::Workload;

use super::context::SimContext;
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

/// Builder pattern for configuring and running simulation experiments.
pub struct SimulationBuilder {
    iteration_control: IterationControl,
    workloads: Vec<Box<dyn Workload>>,
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
            workloads: Vec::new(),
            seeds: Vec::new(),
            use_random_config: false,
            invariants: Vec::new(),
            fault_injectors: Vec::new(),
            phase_config: None,
            exploration_config: None,
        }
    }

    /// Add a workload to the simulation.
    pub fn workload(mut self, w: impl Workload) -> Self {
        self.workloads.push(Box::new(w));
        self
    }

    /// Add a closure-based workload to the simulation.
    pub fn workload_fn<F, Fut>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: FnOnce(&SimContext) -> Fut + 'static,
        Fut: Future<Output = SimulationResult<()>> + 'static,
    {
        self.workloads
            .push(crate::runner::workload::workload_fn(name, f));
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

    #[instrument(skip_all)]
    /// Run the simulation and generate a report.
    pub async fn run(mut self) -> SimulationReport {
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

        // Compute workload name/IP pairs (stable across iterations)
        let workload_info: Vec<(String, String)> = self
            .workloads
            .iter()
            .enumerate()
            .map(|(i, w)| (w.name().to_string(), format!("10.0.0.{}", i + 1)))
            .collect();

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

            // Create fresh NetworkConfiguration for this iteration
            let network_config = if self.use_random_config {
                crate::NetworkConfiguration::random_for_seed()
            } else {
                crate::NetworkConfiguration::default()
            };

            // Create shared SimWorld for this iteration using fresh network config
            let sim = crate::sim::SimWorld::new_with_network_config_and_seed(network_config, seed);

            let start_time = Instant::now();

            // Move workloads and fault injectors to orchestrator, get them back after
            let workloads = std::mem::take(&mut self.workloads);
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
                    // Restore ownership for next iteration
                    self.workloads = returned_workloads;
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

// TODO CLAUDE AI: port to new builder API
/*
#[cfg(test)]
mod tests {
    use super::*;
    use crate::RandomProvider;

    #[test]
    fn test_simulation_builder_basic() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async move {
            SimulationBuilder::new()
                .register_workload(
                    "test_workload",
                    |random, _provider, _time_provider, _task_provider, _topology| async move {
                        Ok(SimulationMetrics {
                            simulated_time: Duration::from_millis(random.random_range(0..100)),
                            events_processed: random.random_range(0..10),
                            ..Default::default()
                        })
                    },
                )
                .set_iterations(3)
                .set_debug_seeds(vec![1, 2, 3])
                .run()
                .await
        });

        assert_eq!(report.iterations, 3);
        assert_eq!(report.successful_runs, 3);
        assert_eq!(report.failed_runs, 0);
        assert_eq!(report.success_rate(), 100.0);

        // Check that seeds were used correctly
        assert_eq!(report.seeds_used, vec![1, 2, 3]);
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
                .register_workload(
                    "failing_workload",
                    |random, _provider, _time_provider, _task_provider, _topology| async move {
                        // Use deterministic approach: fail if random number is even, succeed if odd
                        let random_num = random.random_range(0..100);
                        if random_num % 2 == 0 {
                            Err(crate::SimulationError::InvalidState(
                                "Test failure".to_string(),
                            ))
                        } else {
                            Ok(SimulationMetrics {
                                simulated_time: Duration::from_millis(100),
                                events_processed: 5,
                                ..Default::default()
                            })
                        }
                    },
                )
                .set_iterations(4)
                .set_debug_seeds(vec![1, 2, 5, 6]) // Try different seeds to get 2 even, 2 odd
                .run()
                .await
        });

        assert_eq!(report.iterations, 4);
        assert_eq!(report.successful_runs, 2);
        assert_eq!(report.failed_runs, 2);
        assert_eq!(report.success_rate(), 50.0);

        // Only successful runs should contribute to averages
        assert_eq!(report.average_simulated_time(), Duration::from_millis(100));
        assert_eq!(report.average_events_processed(), 5.0);
    }

    #[tokio::test]
    async fn test_simulation_report_display() {
        let metrics = SimulationMetrics {
            simulated_time: Duration::from_millis(200),
            events_processed: 10,
            ..Default::default()
        };

        let report = SimulationReport {
            iterations: 2,
            successful_runs: 2,
            failed_runs: 0,
            metrics,
            individual_metrics: vec![],
            seeds_used: vec![1, 2],
            seeds_failing: vec![42],
            assertion_results: HashMap::new(),
            assertion_violations: Vec::new(),
            exploration: None,
        };

        let display = format!("{}", report);
        assert!(display.contains("Iterations: 2"));
        assert!(display.contains("Success Rate: 100.00%"));
    }

    #[test]
    fn test_simulation_builder_with_network_config() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async move {
            SimulationBuilder::new()
                .register_workload(
                    "network_test",
                    |_seed, _provider, _time_provider, _task_provider, _topology| async move {
                        Ok(SimulationMetrics {
                            simulated_time: Duration::from_millis(50),
                            events_processed: 10,
                            ..Default::default()
                        })
                    },
                )
                .set_iterations(2)
                .set_debug_seeds(vec![42, 43])
                .run()
                .await
        });

        assert_eq!(report.iterations, 2);
        assert_eq!(report.successful_runs, 2);
        assert_eq!(report.failed_runs, 0);
        assert_eq!(report.success_rate(), 100.0);

        // Verify the network configuration was used by checking if simulation time advanced
        // (WAN config should have higher latencies that cause time advancement)
        assert!(report.average_simulated_time() >= Duration::from_millis(50));
    }

    #[test]
    fn test_multiple_workloads() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async move {
            SimulationBuilder::new()
                .register_workload(
                    "workload1",
                    |random, _provider, _time_provider, _task_provider, _topology| async move {
                        Ok(SimulationMetrics {
                            simulated_time: Duration::from_millis(random.random_range(0..50)),
                            events_processed: random.random_range(0..5),
                            ..Default::default()
                        })
                    },
                )
                .register_workload(
                    "workload2",
                    |random, _provider, _time_provider, _task_provider, _topology| async move {
                        Ok(SimulationMetrics {
                            simulated_time: Duration::from_millis(random.random_range(0..50)),
                            events_processed: random.random_range(0..5),
                            ..Default::default()
                        })
                    },
                )
                .set_iterations(2)
                .set_debug_seeds(vec![10, 20])
                .run()
                .await
        });

        assert_eq!(report.successful_runs, 2);
        assert_eq!(report.failed_runs, 0);
        assert_eq!(report.success_rate(), 100.0);
    }
}
*/
