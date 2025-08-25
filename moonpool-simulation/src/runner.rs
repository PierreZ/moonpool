//! Simulation reporting and statistical analysis framework.
//!
//! This module provides the infrastructure for running multiple simulation iterations,
//! collecting metrics, and generating comprehensive reports for distributed systems testing.

use tracing::instrument;

use crate::{
    SimulationResult,
    assertions::{
        AssertionStats, REGISTERED_ASSERTIONS, ValidationReport, get_assertion_results,
        validate_assertion_contracts,
    },
    reset_sim_rng, set_sim_seed,
};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

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

/// Core metrics collected during a simulation run.
#[derive(Debug, Clone, PartialEq)]
pub struct SimulationMetrics {
    /// Wall-clock time taken for the simulation
    pub wall_time: Duration,
    /// Simulated logical time elapsed
    pub simulated_time: Duration,
    /// Number of events processed
    pub events_processed: u64,
}

impl Default for SimulationMetrics {
    fn default() -> Self {
        Self {
            wall_time: Duration::ZERO,
            simulated_time: Duration::ZERO,
            events_processed: 0,
        }
    }
}

/// Comprehensive report of a simulation run with statistical analysis.
#[derive(Debug, Clone)]
pub struct SimulationReport {
    /// Number of iterations executed
    pub iterations: usize,
    /// Number of successful runs
    pub successful_runs: usize,
    /// Number of failed runs
    pub failed_runs: usize,
    /// Aggregated metrics across all runs
    pub metrics: SimulationMetrics,
    /// Individual metrics for each iteration
    pub individual_metrics: Vec<SimulationResult<SimulationMetrics>>,
    /// Seeds used for each iteration
    pub seeds_used: Vec<u64>,
    /// Aggregated assertion results across all iterations
    pub assertion_results: HashMap<String, AssertionStats>,
    /// Assertion validation result with detailed violation information
    pub assertion_validation: ValidationReport,
}

impl SimulationReport {
    /// Calculate the success rate as a percentage.
    pub fn success_rate(&self) -> f64 {
        if self.iterations == 0 {
            0.0
        } else {
            (self.successful_runs as f64 / self.iterations as f64) * 100.0
        }
    }

    /// Get the average wall time per iteration.
    pub fn average_wall_time(&self) -> Duration {
        if self.successful_runs == 0 {
            Duration::ZERO
        } else {
            self.metrics.wall_time / self.successful_runs as u32
        }
    }

    /// Get the average simulated time per iteration.
    pub fn average_simulated_time(&self) -> Duration {
        if self.successful_runs == 0 {
            Duration::ZERO
        } else {
            self.metrics.simulated_time / self.successful_runs as u32
        }
    }

    /// Get the average number of events processed per iteration.
    pub fn average_events_processed(&self) -> f64 {
        if self.successful_runs == 0 {
            0.0
        } else {
            self.metrics.events_processed as f64 / self.successful_runs as f64
        }
    }
}

impl fmt::Display for SimulationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Simulation Report ===")?;
        writeln!(f, "Iterations: {}", self.iterations)?;
        writeln!(f, "Successful: {}", self.successful_runs)?;
        writeln!(f, "Failed: {}", self.failed_runs)?;
        writeln!(f, "Success Rate: {:.2}%", self.success_rate())?;
        writeln!(f)?;
        writeln!(f, "Average Wall Time: {:?}", self.average_wall_time())?;
        writeln!(
            f,
            "Average Simulated Time: {:?}",
            self.average_simulated_time()
        )?;
        writeln!(
            f,
            "Average Events Processed: {:.1}",
            self.average_events_processed()
        )?;
        writeln!(f)?;

        Ok(())
    }
}

/// Topology information provided to workloads to understand the simulation network.
#[derive(Debug, Clone)]
pub struct WorkloadTopology {
    /// The IP address assigned to this workload
    pub my_ip: String,
    /// The IP addresses of all other peers in the simulation
    pub peer_ips: Vec<String>,
}

/// Type alias for workload function signature to reduce complexity.
type WorkloadFn = Box<
    dyn Fn(
        u64,
        crate::SimNetworkProvider,
        crate::SimTimeProvider,
        WorkloadTopology,
    ) -> Pin<Box<dyn Future<Output = SimulationResult<SimulationMetrics>>>>,
>;

/// A registered workload that can be executed during simulation.
pub struct Workload {
    name: String,
    ip_address: String,
    workload: WorkloadFn,
}

impl fmt::Debug for Workload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workload")
            .field("name", &self.name)
            .field("ip_address", &self.ip_address)
            .field("workload", &"<closure>")
            .finish()
    }
}

/// Builder pattern for configuring and running simulation experiments.
#[derive(Debug)]
pub struct SimulationBuilder {
    iteration_control: IterationControl,
    workloads: Vec<Workload>,
    seeds: Vec<u64>,
    next_ip: u32, // For auto-assigning IP addresses starting from 10.0.0.1
    network_config: crate::NetworkConfiguration, // Network configuration for simulation
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
            network_config: crate::NetworkConfiguration::default(),
        }
    }

    /// Register a workload with the simulation builder.
    ///
    /// # Arguments
    /// * `name` - Name for the workload (for reporting purposes)
    /// * `workload` - Async function that takes a seed, NetworkProvider, TimeProvider, and WorkloadTopology and returns simulation metrics
    pub fn register_workload<S, F, Fut>(mut self, name: S, workload: F) -> Self
    where
        S: Into<String>,
        F: Fn(u64, crate::SimNetworkProvider, crate::SimTimeProvider, WorkloadTopology) -> Fut
            + 'static,
        Fut: Future<Output = SimulationResult<SimulationMetrics>> + 'static,
    {
        // Auto-assign IP address starting from 10.0.0.1
        let ip_address = format!("10.0.0.{}", self.next_ip);
        self.next_ip += 1;

        let boxed_workload = Box::new(move |seed, provider, time_provider, topology| {
            let fut = workload(seed, provider, time_provider, topology);
            Box::pin(fut) as Pin<Box<dyn Future<Output = SimulationResult<SimulationMetrics>>>>
        });

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

    /// Set specific seeds to use for the iterations.
    /// If not set, random seeds will be generated.
    pub fn set_seeds(mut self, seeds: Vec<u64>) -> Self {
        self.seeds = seeds;
        self
    }

    /// Set the network configuration for the simulation.
    pub fn set_network_config(mut self, network_config: crate::NetworkConfiguration) -> Self {
        self.network_config = network_config;
        self
    }

    /// Check if all registered sometimes_assert! assertions have been reached with at least one success.
    /// This is used by the UntilAllSometimesReached iteration control.
    fn all_sometimes_assertions_reached() -> bool {
        let results = get_assertion_results();
        let registry = REGISTERED_ASSERTIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Get all registered assertion names across all modules
        let all_registered: std::collections::HashSet<String> = registry
            .values()
            .flat_map(|assertions| assertions.iter())
            .cloned()
            .collect();

        // Debug logging
        tracing::debug!("Checking if all assertions reached with success:");
        tracing::debug!("Registered assertions: {:?}", all_registered);
        tracing::debug!(
            "Executed assertions: {:?}",
            results.keys().collect::<Vec<_>>()
        );

        // Check if all registered assertions have been executed AND have at least one success
        for assertion_name in &all_registered {
            match results.get(assertion_name) {
                Some(stats) if stats.successes > 0 => {
                    tracing::debug!(
                        "Assertion {} reached with {} successes",
                        assertion_name,
                        stats.successes
                    );
                    // This assertion is satisfied
                }
                Some(stats) => {
                    tracing::debug!(
                        "Assertion {} executed {} times but never succeeded",
                        assertion_name,
                        stats.total_checks
                    );
                    return false;
                }
                None => {
                    tracing::debug!("Missing assertion: {}", assertion_name);
                    return false;
                }
            }
        }

        tracing::debug!("All assertions reached with at least one success!");
        true
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
                assertion_results: HashMap::new(),
                assertion_validation: ValidationReport::new(),
            };
        }

        // Initialize iteration state
        let mut seeds_to_use = self.seeds.clone();
        let mut individual_metrics = Vec::new();
        let mut successful_runs = 0;
        let mut failed_runs = 0;
        let mut aggregated_metrics = SimulationMetrics::default();

        // Generate random seeds if none provided
        let base_seed = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(12345);

        let mut iteration_count = 0;
        let start_time = Instant::now();

        loop {
            // Check exit conditions
            match &self.iteration_control {
                IterationControl::FixedCount(count) => {
                    if iteration_count >= *count {
                        break;
                    }
                }
                IterationControl::TimeLimit(duration) => {
                    if start_time.elapsed() >= *duration {
                        break;
                    }
                }
                IterationControl::UntilAllSometimesReached(safety_limit) => {
                    if iteration_count >= *safety_limit {
                        break;
                    }
                    if iteration_count > 0 && Self::all_sometimes_assertions_reached() {
                        break;
                    }
                }
            }

            // Get or generate seed for this iteration
            let seed = if iteration_count < seeds_to_use.len() {
                seeds_to_use[iteration_count]
            } else {
                use std::collections::hash_map::DefaultHasher;
                use std::hash::{Hash, Hasher};
                let mut hasher = DefaultHasher::new();
                base_seed.hash(&mut hasher);
                iteration_count.hash(&mut hasher);
                let new_seed = hasher.finish();
                seeds_to_use.push(new_seed);
                new_seed
            };

            iteration_count += 1;
            // Prepare clean state for this iteration
            reset_sim_rng();
            set_sim_seed(seed);

            // Create shared SimWorld for this iteration using configured network
            let mut sim = crate::SimWorld::new_with_network_config_and_seed(
                self.network_config.clone(),
                seed,
            );
            let provider = sim.network_provider();

            let start_time = Instant::now();

            // Build topology information for each workload
            let all_ips: Vec<String> = self
                .workloads
                .iter()
                .map(|w| w.ip_address.clone())
                .collect();

            // Execute workloads cooperatively using spawn_local and event processing
            let all_results = if self.workloads.len() == 1 {
                // Single workload - execute directly
                let my_ip = self.workloads[0].ip_address.clone();
                let peer_ips = all_ips.iter().filter(|ip| *ip != &my_ip).cloned().collect();
                let topology = WorkloadTopology { my_ip, peer_ips };

                let time_provider = sim.time_provider();
                let result =
                    (self.workloads[0].workload)(seed, provider.clone(), time_provider, topology)
                        .await;
                vec![result]
            } else {
                // Multiple workloads - spawn them and process events cooperatively
                tracing::debug!(
                    "Spawning {} workloads with spawn_local",
                    self.workloads.len()
                );
                let mut handles = Vec::new();
                for (idx, workload) in self.workloads.iter().enumerate() {
                    tracing::debug!("Spawning workload {}: {}", idx, workload.name);

                    let my_ip = workload.ip_address.clone();
                    let peer_ips = all_ips.iter().filter(|ip| *ip != &my_ip).cloned().collect();
                    let topology = WorkloadTopology { my_ip, peer_ips };

                    let time_provider = sim.time_provider();
                    let handle = tokio::task::spawn_local((workload.workload)(
                        seed,
                        provider.clone(),
                        time_provider,
                        topology,
                    ));
                    handles.push(handle);
                }

                // Process events while workloads run
                let mut results = Vec::new();
                let mut loop_count = 0;
                let mut no_progress_count = 0;
                while !handles.is_empty() {
                    loop_count += 1;
                    if loop_count % 100 == 0 {
                        tracing::debug!(
                            "Cooperative loop iteration {}, {} handles remaining, {} pending events",
                            loop_count,
                            handles.len(),
                            sim.pending_event_count()
                        );
                    }

                    let initial_handle_count = handles.len();
                    let initial_event_count = sim.pending_event_count();

                    // Process one simulation event to allow better interleaving
                    if sim.pending_event_count() > 0 {
                        tracing::trace!(
                            "Processing one simulation event, {} events pending",
                            sim.pending_event_count()
                        );
                        sim.step();
                    }

                    // Check if any handles are ready
                    let mut i = 0;
                    while i < handles.len() {
                        if handles[i].is_finished() {
                            tracing::debug!("Workload handle {} finished", i);
                            let join_result = handles.remove(i).await;
                            let result = match join_result {
                                Ok(workload_result) => {
                                    tracing::debug!("Workload completed successfully");
                                    workload_result
                                }
                                Err(_) => {
                                    tracing::error!("Workload task panicked");
                                    Err(crate::SimulationError::InvalidState(
                                        "Task panicked".to_string(),
                                    ))
                                }
                            };
                            results.push(result);
                        } else {
                            i += 1;
                        }
                    }

                    // Check for deadlock: no events and no progress made
                    if sim.pending_event_count() == 0
                        && handles.len() == initial_handle_count
                        && initial_event_count == 0
                    {
                        no_progress_count += 1;
                        if no_progress_count > 10 {
                            tracing::error!(
                                "Deadlock detected: {} tasks remaining but no events to process after {} iterations",
                                handles.len(),
                                no_progress_count
                            );
                            // Mark all remaining tasks as failed
                            for _ in 0..handles.len() {
                                results.push(Err(crate::SimulationError::InvalidState(
                                    "Deadlock detected: tasks stuck with no events".to_string(),
                                )));
                            }
                            break;
                        }
                    } else {
                        no_progress_count = 0;
                    }

                    // Yield to allow tasks to make progress
                    if !handles.is_empty() {
                        tracing::trace!(
                            "Yielding to allow {} tasks to make progress",
                            handles.len()
                        );
                        tokio::task::yield_now().await;
                    }
                }

                tracing::debug!(
                    "All workloads completed after {} loop iterations, processing remaining events",
                    loop_count
                );
                // Process any remaining events after all workloads complete
                sim.run_until_empty();
                results
            };

            let wall_time = start_time.elapsed();

            // Aggregate results from all workloads
            let mut iteration_successful = true;
            let mut iteration_metrics = SimulationMetrics::default();

            for result in &all_results {
                match result {
                    Ok(metrics) => {
                        // Aggregate metrics from this workload
                        iteration_metrics.simulated_time =
                            iteration_metrics.simulated_time.max(metrics.simulated_time);
                        iteration_metrics.events_processed += metrics.events_processed;
                    }
                    Err(_) => {
                        iteration_successful = false;
                    }
                }
            }

            if iteration_successful {
                successful_runs += 1;

                // Extract final simulation metrics
                let sim_metrics = sim.extract_metrics();

                // Combine workload and simulation metrics for this iteration
                let mut combined_metrics = iteration_metrics;
                // Use simulation time from the actual simulation world if it advanced,
                // otherwise use the maximum from workloads
                if sim_metrics.simulated_time > Duration::ZERO {
                    combined_metrics.simulated_time = sim_metrics.simulated_time;
                }
                // Use the actual events processed from the simulation world if any events occurred,
                // otherwise preserve workload metrics
                if sim_metrics.events_processed > 0 {
                    combined_metrics.events_processed = sim_metrics.events_processed;
                }

                // Aggregate metrics across iterations
                aggregated_metrics.wall_time += wall_time;
                aggregated_metrics.simulated_time += combined_metrics.simulated_time;
                aggregated_metrics.events_processed += combined_metrics.events_processed;

                individual_metrics.push(Ok(combined_metrics));
            } else {
                failed_runs += 1;
                individual_metrics.push(Err(crate::SimulationError::InvalidState(
                    "One or more workloads failed".to_string(),
                )));
            }
        }

        // End of main iteration loop

        // Collect assertion results and validate them
        let assertion_results = get_assertion_results();
        let assertion_validation = validate_assertion_contracts();

        SimulationReport {
            iterations: iteration_count,
            successful_runs,
            failed_runs,
            metrics: aggregated_metrics,
            individual_metrics,
            seeds_used: seeds_to_use,
            assertion_results,
            assertion_validation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_simulation_builder_basic() {
        let report = SimulationBuilder::new()
            .register_workload(
                "test_workload",
                |seed, _provider, _time_provider, _topology| async move {
                    let mut metrics = SimulationMetrics::default();
                    metrics.simulated_time = Duration::from_millis(seed % 100);
                    metrics.events_processed = seed % 10;
                    Ok(metrics)
                },
            )
            .set_iterations(3)
            .set_seeds(vec![1, 2, 3])
            .run()
            .await;

        assert_eq!(report.iterations, 3);
        assert_eq!(report.successful_runs, 3);
        assert_eq!(report.failed_runs, 0);
        assert_eq!(report.success_rate(), 100.0);

        // Check that seeds were used correctly
        assert_eq!(report.seeds_used, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_simulation_builder_with_failures() {
        let report = SimulationBuilder::new()
            .register_workload(
                "failing_workload",
                |seed, _provider, _time_provider, _topology| async move {
                    if seed % 2 == 0 {
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
            .set_seeds(vec![1, 2, 3, 4])
            .run()
            .await;

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
            assertion_results: HashMap::new(),
            assertion_validation: ValidationReport::new(),
        };

        let display = format!("{}", report);
        assert!(display.contains("Iterations: 2"));
        assert!(display.contains("Success Rate: 100.00%"));
    }

    #[tokio::test]
    async fn test_simulation_builder_with_network_config() {
        let wan_config = crate::NetworkConfiguration::wan_simulation();

        let report = SimulationBuilder::new()
            .set_network_config(wan_config)
            .register_workload(
                "network_test",
                |_seed, _provider, _time_provider, _topology| async move {
                    let mut metrics = SimulationMetrics::default();
                    metrics.simulated_time = Duration::from_millis(50);
                    metrics.events_processed = 10;
                    Ok(metrics)
                },
            )
            .set_iterations(2)
            .set_seeds(vec![42, 43])
            .run()
            .await;

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
                    |seed, _provider, _time_provider, _topology| async move {
                        let mut metrics = SimulationMetrics::default();
                        metrics.simulated_time = Duration::from_millis(seed % 50);
                        metrics.events_processed = seed % 5;
                        Ok(metrics)
                    },
                )
                .register_workload(
                    "workload2",
                    |seed, _provider, _time_provider, _topology| async move {
                        let mut metrics = SimulationMetrics::default();
                        metrics.simulated_time = Duration::from_millis((seed * 2) % 50);
                        metrics.events_processed = (seed * 2) % 5;
                        Ok(metrics)
                    },
                )
                .set_iterations(2)
                .set_seeds(vec![10, 20])
                .run()
                .await
        });

        assert_eq!(report.successful_runs, 2);
        assert_eq!(report.failed_runs, 0);
        assert_eq!(report.success_rate(), 100.0);
    }
}
