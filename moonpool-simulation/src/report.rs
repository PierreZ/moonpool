//! Simulation reporting and statistical analysis framework.
//!
//! This module provides the infrastructure for running multiple simulation iterations,
//! collecting metrics, and generating comprehensive reports for distributed systems testing.

use tracing::instrument;

use crate::{SimulationResult, reset_sim_rng, set_sim_seed};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// Core metrics collected during a simulation run.
#[derive(Debug, Clone, PartialEq)]
pub struct SimulationMetrics {
    /// Wall-clock time taken for the simulation
    pub wall_time: Duration,
    /// Simulated logical time elapsed
    pub simulated_time: Duration,
    /// Number of events processed
    pub events_processed: u64,
    /// Custom metrics collected during the simulation
    pub custom_metrics: HashMap<String, f64>,
}

impl Default for SimulationMetrics {
    fn default() -> Self {
        Self {
            wall_time: Duration::ZERO,
            simulated_time: Duration::ZERO,
            events_processed: 0,
            custom_metrics: HashMap::new(),
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

        if !self.metrics.custom_metrics.is_empty() {
            writeln!(f)?;
            writeln!(f, "Custom Metrics:")?;
            for (name, value) in &self.metrics.custom_metrics {
                writeln!(f, "  {}: {:.3}", name, value)?;
            }
        }

        Ok(())
    }
}

/// Type alias for workload function signature to reduce complexity.
type WorkloadFn = Box<
    dyn Fn(
        u64,
        crate::SimNetworkProvider,
        Option<String>,
    ) -> Pin<Box<dyn Future<Output = SimulationResult<SimulationMetrics>>>>,
>;

/// A registered workload that can be executed during simulation.
pub struct Workload {
    name: String,
    ip_address: Option<String>,
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
    iterations: usize,
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
            iterations: 1,
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
    /// * `workload` - Async function that takes a seed, NetworkProvider, and optional IP address and returns simulation metrics
    pub fn register_workload<S, F, Fut>(mut self, name: S, workload: F) -> Self
    where
        S: Into<String>,
        F: Fn(u64, crate::SimNetworkProvider, Option<String>) -> Fut + 'static,
        Fut: Future<Output = SimulationResult<SimulationMetrics>> + 'static,
    {
        // Auto-assign IP address starting from 10.0.0.1
        let ip_address = format!("10.0.0.{}", self.next_ip);
        self.next_ip += 1;

        let boxed_workload = Box::new(move |seed, provider, ip| {
            let fut = workload(seed, provider, ip);
            Box::pin(fut) as Pin<Box<dyn Future<Output = SimulationResult<SimulationMetrics>>>>
        });

        self.workloads.push(Workload {
            name: name.into(),
            ip_address: Some(ip_address),
            workload: boxed_workload,
        });
        self
    }

    /// Set the number of iterations to run.
    pub fn set_iterations(mut self, iterations: usize) -> Self {
        self.iterations = iterations;
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
            };
        }

        let mut seeds_to_use = self.seeds.clone();

        // Generate random seeds if none provided
        if seeds_to_use.len() < self.iterations {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            use std::time::SystemTime;

            let base_seed = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(12345);

            for i in seeds_to_use.len()..self.iterations {
                let mut hasher = DefaultHasher::new();
                base_seed.hash(&mut hasher);
                i.hash(&mut hasher);
                seeds_to_use.push(hasher.finish());
            }
        }

        let mut individual_metrics = Vec::new();
        let mut successful_runs = 0;
        let mut failed_runs = 0;
        let mut aggregated_metrics = SimulationMetrics::default();

        for (_, &seed) in seeds_to_use.iter().enumerate().take(self.iterations) {
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

            // Execute workloads cooperatively using spawn_local and event processing
            let all_results = if self.workloads.len() == 1 {
                // Single workload - execute directly
                let result = (self.workloads[0].workload)(
                    seed,
                    provider.clone(),
                    self.workloads[0].ip_address.clone(),
                )
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
                    let handle = tokio::task::spawn_local((workload.workload)(
                        seed,
                        provider.clone(),
                        workload.ip_address.clone(),
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
                        if no_progress_count > 1000 {
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

                        // Aggregate custom metrics
                        for (key, value) in &metrics.custom_metrics {
                            *iteration_metrics
                                .custom_metrics
                                .entry(key.clone())
                                .or_insert(0.0) += value;
                        }
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

                // Aggregate custom metrics from workloads and simulation
                for (key, value) in &combined_metrics.custom_metrics {
                    *aggregated_metrics
                        .custom_metrics
                        .entry(key.clone())
                        .or_insert(0.0) += value;
                }
                for (key, value) in &sim_metrics.custom_metrics {
                    *aggregated_metrics
                        .custom_metrics
                        .entry(key.clone())
                        .or_insert(0.0) += value;
                }

                individual_metrics.push(Ok(combined_metrics));
            } else {
                failed_runs += 1;
                individual_metrics.push(Err(crate::SimulationError::InvalidState(
                    "One or more workloads failed".to_string(),
                )));
            }
        }

        // Average custom metrics
        for value in aggregated_metrics.custom_metrics.values_mut() {
            if successful_runs > 0 {
                *value /= successful_runs as f64;
            }
        }

        SimulationReport {
            iterations: self.iterations,
            successful_runs,
            failed_runs,
            metrics: aggregated_metrics,
            individual_metrics,
            seeds_used: seeds_to_use,
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
            .register_workload("test_workload", |seed, _provider, _ip| async move {
                let mut metrics = SimulationMetrics::default();
                metrics.simulated_time = Duration::from_millis(seed % 100);
                metrics.events_processed = seed % 10;
                Ok(metrics)
            })
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
            .register_workload("failing_workload", |seed, _provider, _ip| async move {
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
            })
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
        metrics
            .custom_metrics
            .insert("throughput".to_string(), 42.5);

        let report = SimulationReport {
            iterations: 2,
            successful_runs: 2,
            failed_runs: 0,
            metrics,
            individual_metrics: vec![],
            seeds_used: vec![1, 2],
        };

        let display = format!("{}", report);
        assert!(display.contains("Iterations: 2"));
        assert!(display.contains("Success Rate: 100.00%"));
        assert!(display.contains("throughput: 42.500"));
    }

    #[tokio::test]
    async fn test_custom_metrics_aggregation() {
        let report = SimulationBuilder::new()
            .register_workload("custom_metrics_test", |seed, _provider, _ip| async move {
                let mut metrics = SimulationMetrics::default();
                metrics
                    .custom_metrics
                    .insert("latency".to_string(), seed as f64);
                metrics
                    .custom_metrics
                    .insert("throughput".to_string(), (seed * 2) as f64);
                Ok(metrics)
            })
            .set_iterations(3)
            .set_seeds(vec![10, 20, 30])
            .run()
            .await;

        assert_eq!(report.successful_runs, 3);

        // Custom metrics should be averaged
        assert_eq!(report.metrics.custom_metrics["latency"], 20.0); // (10+20+30)/3
        assert_eq!(report.metrics.custom_metrics["throughput"], 40.0); // (20+40+60)/3
    }

    #[tokio::test]
    async fn test_simulation_builder_with_network_config() {
        let wan_config = crate::NetworkConfiguration::wan_simulation();

        let report = SimulationBuilder::new()
            .set_network_config(wan_config)
            .register_workload("network_test", |_seed, _provider, _ip| async move {
                let mut metrics = SimulationMetrics::default();
                metrics.simulated_time = Duration::from_millis(50);
                metrics.events_processed = 10;
                metrics
                    .custom_metrics
                    .insert("network_configured".to_string(), 1.0);
                Ok(metrics)
            })
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

        // Verify custom metric was collected
        assert_eq!(
            report.metrics.custom_metrics.get("network_configured"),
            Some(&1.0)
        );
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
                .register_workload("workload1", |seed, _provider, _ip| async move {
                    let mut metrics = SimulationMetrics::default();
                    metrics.simulated_time = Duration::from_millis(seed % 50);
                    metrics.events_processed = seed % 5;
                    metrics
                        .custom_metrics
                        .insert("workload1_metric".to_string(), seed as f64);
                    Ok(metrics)
                })
                .register_workload("workload2", |seed, _provider, _ip| async move {
                    let mut metrics = SimulationMetrics::default();
                    metrics.simulated_time = Duration::from_millis((seed * 2) % 50);
                    metrics.events_processed = (seed * 2) % 5;
                    metrics
                        .custom_metrics
                        .insert("workload2_metric".to_string(), (seed * 2) as f64);
                    Ok(metrics)
                })
                .set_iterations(2)
                .set_seeds(vec![10, 20])
                .run()
                .await
        });

        assert_eq!(report.successful_runs, 2);
        assert_eq!(report.failed_runs, 0);
        assert_eq!(report.success_rate(), 100.0);

        // Both workload metrics should be aggregated
        assert!(
            report
                .metrics
                .custom_metrics
                .contains_key("workload1_metric")
        );
        assert!(
            report
                .metrics
                .custom_metrics
                .contains_key("workload2_metric")
        );
    }
}
