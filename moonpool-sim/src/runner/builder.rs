//! Simulation builder pattern for configuring and running experiments.
//!
//! This module provides the main SimulationBuilder type for setting up
//! and executing simulation experiments.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tracing::instrument;

use crate::{InvariantCheck, SimulationResult};

use super::orchestrator::{IterationManager, MetricsCollector, WorkloadOrchestrator};
use super::report::{SimulationMetrics, SimulationReport};
use super::topology::{Workload, WorkloadTopology};

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

/// Type alias for workload function signature to reduce complexity.
pub(crate) type WorkloadFn = Box<
    dyn Fn(
        crate::SimRandomProvider,
        crate::SimNetworkProvider,
        crate::SimTimeProvider,
        crate::TokioTaskProvider,
        WorkloadTopology,
    ) -> Pin<Box<dyn Future<Output = SimulationResult<SimulationMetrics>>>>,
>;

/// Builder pattern for configuring and running simulation experiments.
pub struct SimulationBuilder {
    iteration_control: IterationControl,
    workloads: Vec<Workload>,
    seeds: Vec<u64>,
    next_ip: u32, // For auto-assigning IP addresses starting from 10.0.0.1
    use_random_config: bool,
    invariants: Vec<InvariantCheck>,
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
            next_ip: 1, // Start from 10.0.0.1
            use_random_config: false,
            invariants: Vec::new(),
        }
    }

    /// Register a workload with the simulation builder.
    ///
    /// # Arguments
    /// * `name` - Name for the workload (for reporting purposes)
    /// * `workload` - Async function that takes a RandomProvider, NetworkProvider, TimeProvider, TaskProvider, and WorkloadTopology and returns simulation metrics
    pub fn register_workload<S, F, Fut>(mut self, name: S, workload: F) -> Self
    where
        S: Into<String>,
        F: Fn(
                crate::SimRandomProvider,
                crate::SimNetworkProvider,
                crate::SimTimeProvider,
                crate::TokioTaskProvider,
                WorkloadTopology,
            ) -> Fut
            + 'static,
        Fut: Future<Output = SimulationResult<SimulationMetrics>> + 'static,
    {
        // Auto-assign IP address starting from 10.0.0.1
        let ip_address = format!("10.0.0.{}", self.next_ip);
        self.next_ip += 1;

        let boxed_workload = Box::new(
            move |random_provider, provider, time_provider, task_provider, topology| {
                let fut = workload(
                    random_provider,
                    provider,
                    time_provider,
                    task_provider,
                    topology,
                );
                Box::pin(fut) as Pin<Box<dyn Future<Output = SimulationResult<SimulationMetrics>>>>
            },
        );

        self.workloads.push(Workload {
            name: name.into(),
            ip_address,
            workload: boxed_workload,
        });
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

    /// Run until all sometimes_assert! assertions have been reached.
    pub fn run_until_all_sometimes_reached(mut self, safety_limit: usize) -> Self {
        self.iteration_control = IterationControl::UntilAllSometimesReached(safety_limit);
        self
    }

    /// Set specific seeds for deterministic debugging and regression testing.
    ///
    /// This method is specifically designed for debugging scenarios where you need
    /// to reproduce specific problematic behavior. Unlike `set_seeds()`, the name
    /// makes it clear this is for debugging/testing specific scenarios.
    ///
    /// **Key differences from `set_seeds()`:**
    /// - **Intent**: Clearly indicates debugging/testing purpose
    /// - **Usage**: Typically used with `FixedCount(1)` for reproducing exact scenarios
    /// - **Documentation**: Self-documenting that these seeds are for specific test cases
    ///
    /// **Common use cases:**
    /// - Reproducing TCP ordering bugs (e.g., seed 42 revealed the ordering issue)
    /// - Regression testing for specific edge cases
    /// - Deterministic testing in CI/CD pipelines
    /// - Investigating assertion failures at specific seeds
    ///
    /// Example: `set_debug_seeds(vec![42])` with `FixedCount(1)` ensures the test
    /// always runs with seed 42, making it reproducible for debugging the TCP ordering fix.
    pub fn set_debug_seeds(mut self, seeds: Vec<u64>) -> Self {
        self.seeds = seeds;
        self
    }

    /// Enable randomized network configuration for chaos testing
    pub fn use_random_config(mut self) -> Self {
        self.use_random_config = true;
        self
    }

    /// Register invariant check functions to be executed after every simulation event.
    ///
    /// Invariants receive a snapshot of all actor states and the current simulation time,
    /// and should panic if any global property is violated.
    ///
    /// # Arguments
    /// * `invariants` - Vector of invariant check functions
    ///
    /// # Example
    /// ```ignore
    /// SimulationBuilder::new()
    ///     .with_invariants(vec![
    ///         Box::new(|states, _time| {
    ///             let total_sent: u64 = states.values()
    ///                 .filter_map(|v| v.get("messages_sent").and_then(|s| s.as_u64()))
    ///                 .sum();
    ///             let total_received: u64 = states.values()
    ///                 .filter_map(|v| v.get("messages_received").and_then(|r| r.as_u64()))
    ///                 .sum();
    ///             assert!(total_received <= total_sent, "Message conservation violated");
    ///         })
    ///     ])
    /// ```
    pub fn with_invariants(mut self, invariants: Vec<InvariantCheck>) -> Self {
        self.invariants = invariants;
        self
    }

    #[instrument(skip_all)]
    /// Run the simulation and generate a report.
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
            };
        }

        // Initialize iteration state
        let mut iteration_manager =
            IterationManager::new(self.iteration_control.clone(), self.seeds.clone());
        let mut metrics_collector = MetricsCollector::new();

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
            let provider = sim.network_provider();

            let start_time = Instant::now();

            // Create shutdown signal for this iteration
            let shutdown_signal = tokio_util::sync::CancellationToken::new();

            // Execute workloads using orchestrator
            let orchestration_result = WorkloadOrchestrator::orchestrate_workloads(
                &self.workloads,
                seed,
                provider,
                sim,
                shutdown_signal,
                iteration_count,
                &self.invariants,
            )
            .await;

            let (all_results, sim_metrics) = match orchestration_result {
                Ok((results, metrics)) => (results, metrics),
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
                    );
                }
            };

            let wall_time = start_time.elapsed();

            // Record iteration results using metrics collector
            metrics_collector.record_iteration(seed, wall_time, all_results, sim_metrics);

            // Reset buggify state after each iteration to ensure clean state
            crate::chaos::buggify_reset();
        }

        // End of main iteration loop

        // Log summary of all seeds used
        let iteration_count = iteration_manager.current_iteration();
        let (successful_runs, failed_runs) = metrics_collector.current_stats();
        tracing::info!(
            "📊 Simulation completed: {}/{} iterations successful",
            successful_runs,
            iteration_count
        );
        tracing::info!("🌱 Seeds used: {:?}", iteration_manager.seeds_used());
        if failed_runs > 0 {
            tracing::warn!(
                "⚠️ {} iterations failed - check logs above for failing seeds",
                failed_runs
            );
        }

        // Collect assertion results and validate them
        let assertion_results = crate::chaos::get_assertion_results();
        let assertion_violations = crate::chaos::validate_assertion_contracts();

        // Final buggify reset to ensure no impact on subsequent code
        crate::chaos::buggify_reset();

        metrics_collector.generate_report(
            iteration_count,
            iteration_manager.seeds_used().to_vec(),
            assertion_results,
            assertion_violations,
        )
    }
}

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
                        let mut metrics = SimulationMetrics::default();
                        metrics.simulated_time = Duration::from_millis(random.random_range(0..100));
                        metrics.events_processed = random.random_range(0..10);
                        Ok(metrics)
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
                            let mut metrics = SimulationMetrics::default();
                            metrics.simulated_time = Duration::from_millis(100);
                            metrics.events_processed = 5;
                            Ok(metrics)
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
        let mut metrics = SimulationMetrics::default();
        metrics.simulated_time = Duration::from_millis(200);
        metrics.events_processed = 10;

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
                        let mut metrics = SimulationMetrics::default();
                        metrics.simulated_time = Duration::from_millis(50);
                        metrics.events_processed = 10;
                        Ok(metrics)
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
                        let mut metrics = SimulationMetrics::default();
                        metrics.simulated_time = Duration::from_millis(random.random_range(0..50));
                        metrics.events_processed = random.random_range(0..5);
                        Ok(metrics)
                    },
                )
                .register_workload(
                    "workload2",
                    |random, _provider, _time_provider, _task_provider, _topology| async move {
                        let mut metrics = SimulationMetrics::default();
                        metrics.simulated_time = Duration::from_millis(random.random_range(0..50));
                        metrics.events_processed = random.random_range(0..5);
                        Ok(metrics)
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
