//! Simulation reporting and statistical analysis framework.
//!
//! This module provides the infrastructure for running multiple simulation iterations,
//! collecting metrics, and generating comprehensive reports for distributed systems testing.

use tracing::instrument;

use crate::{
    SimulationResult,
    assertions::{AssertionStats, get_assertion_results, validate_assertion_contracts},
    buggify::{buggify_init, buggify_reset},
    reset_sim_rng, set_sim_seed,
};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// Deadlock detection utility to identify stuck simulations.
#[derive(Debug, Default)]
struct DeadlockDetector {
    no_progress_count: usize,
    threshold: usize,
}

impl DeadlockDetector {
    /// Create a new deadlock detector with a threshold for consecutive no-progress iterations.
    fn new(threshold: usize) -> Self {
        Self {
            no_progress_count: 0,
            threshold,
        }
    }

    /// Check if deadlock conditions are met and update internal state.
    /// Returns true if deadlock is detected.
    fn check_deadlock(
        &mut self,
        handles_count: usize,
        initial_handle_count: usize,
        event_count: usize,
        initial_event_count: usize,
    ) -> bool {
        // Check for deadlock: no events and no progress made
        if event_count == 0 && handles_count == initial_handle_count && initial_event_count == 0 {
            self.no_progress_count += 1;
            self.no_progress_count > self.threshold
        } else {
            self.no_progress_count = 0;
            false
        }
    }

    /// Get the current no-progress count for logging.
    fn no_progress_count(&self) -> usize {
        self.no_progress_count
    }
}

/// Orchestrates workload execution and event processing.
struct WorkloadOrchestrator;

impl WorkloadOrchestrator {
    /// Execute all workloads using spawn_local and coordinate their execution.
    async fn orchestrate_workloads(
        workloads: &[Workload],
        seed: u64,
        provider: crate::SimNetworkProvider,
        mut sim: crate::SimWorld,
        shutdown_signal: tokio_util::sync::CancellationToken,
        iteration_count: usize,
    ) -> Result<(Vec<SimulationResult<SimulationMetrics>>, SimulationMetrics), (Vec<u64>, usize)>
    {
        tracing::debug!("Spawning {} workload(s) with spawn_local", workloads.len());
        let mut handles = Vec::new();
        for (idx, workload) in workloads.iter().enumerate() {
            tracing::debug!("Spawning workload {}: {}", idx, workload.name);

            let topology = TopologyFactory::create_topology(
                &workload.ip_address,
                workloads,
                shutdown_signal.clone(),
            );

            let time_provider = sim.time_provider();
            let task_provider = sim.task_provider();
            // Create random provider from seed
            let random_provider = crate::random::sim::SimRandomProvider::new(seed);
            let handle = tokio::task::spawn_local((workload.workload)(
                random_provider,
                provider.clone(),
                time_provider,
                task_provider,
                topology,
            ));
            handles.push(handle);
        }

        // Process events while workloads run
        let mut results = Vec::new();
        let mut loop_count = 0;
        let mut deadlock_detector = DeadlockDetector::new(3);
        let mut first_success_triggered = false;
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

                    // If this is the first successful workload, trigger shutdown signal
                    if !first_success_triggered && result.is_ok() {
                        Self::trigger_shutdown(&mut sim, &shutdown_signal);
                        first_success_triggered = true;
                    }

                    results.push(result);
                } else {
                    i += 1;
                }
            }

            // Check for deadlock using dedicated detector
            if deadlock_detector.check_deadlock(
                handles.len(),
                initial_handle_count,
                sim.pending_event_count(),
                initial_event_count,
            ) {
                tracing::error!(
                    "🔒 DEADLOCK detected on iteration {} with seed {}: {} tasks remaining but no events to process after {} iterations",
                    iteration_count,
                    seed,
                    handles.len(),
                    deadlock_detector.no_progress_count()
                );
                // Mark all remaining tasks as failed
                for _ in 0..handles.len() {
                    results.push(Err(crate::SimulationError::InvalidState(
                        format!("Deadlock detected on iteration {} with seed {}: tasks stuck with no events", iteration_count, seed),
                    )));
                }

                // Return error state for early exit
                return Err((vec![seed], 1));
            }

            // Yield to allow tasks to make progress
            if !handles.is_empty() {
                tracing::trace!("Yielding to allow {} tasks to make progress", handles.len());
                tokio::task::yield_now().await;
            }
        }

        tracing::debug!(
            "All workloads completed after {} loop iterations, processing remaining events",
            loop_count
        );
        // Process any remaining events after all workloads complete
        sim.run_until_empty();

        // Extract final simulation metrics
        let sim_metrics = sim.extract_metrics();

        Ok((results, sim_metrics))
    }

    /// Trigger shutdown signal and schedule wake events.
    fn trigger_shutdown(
        sim: &mut crate::SimWorld,
        shutdown_signal: &tokio_util::sync::CancellationToken,
    ) {
        tracing::debug!("First workload completed successfully, triggering shutdown signal");
        shutdown_signal.cancel();

        // Schedule a shutdown event to wake all tasks
        tracing::debug!("Scheduling Shutdown event to wake all tasks");
        sim.schedule_event(crate::Event::Shutdown, Duration::from_nanos(1));

        // Schedule many periodic wake events to ensure tasks can check shutdown
        for i in 1..100 {
            sim.schedule_event(
                crate::Event::Timer {
                    task_id: u64::MAX - i,
                },
                Duration::from_nanos(i),
            );
        }
    }
}

/// Manages iteration control, seed generation, and progress tracking.
struct IterationManager {
    control: IterationControl,
    seeds: Vec<u64>,
    base_seed: u64,
    iteration_count: usize,
    start_time: Instant,
}

impl IterationManager {
    /// Create a new iteration manager with the given control strategy and initial seeds.
    fn new(control: IterationControl, initial_seeds: Vec<u64>) -> Self {
        let base_seed = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(12345);

        Self {
            control,
            seeds: initial_seeds,
            base_seed,
            iteration_count: 0,
            start_time: Instant::now(),
        }
    }

    /// Check if more iterations should be run.
    fn should_continue(&self) -> bool {
        match &self.control {
            IterationControl::FixedCount(count) => self.iteration_count < *count,
            IterationControl::TimeLimit(duration) => self.start_time.elapsed() < *duration,
            IterationControl::UntilAllSometimesReached(safety_limit) => {
                self.iteration_count < *safety_limit
                    && !(self.iteration_count > 0 && Self::all_sometimes_assertions_reached())
            }
        }
    }

    /// Get the seed for the current iteration and advance to the next.
    fn next_iteration(&mut self) -> u64 {
        let seed = if self.iteration_count < self.seeds.len() {
            self.seeds[self.iteration_count]
        } else {
            // Generate new seed using hash of base seed and iteration count
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            self.base_seed.hash(&mut hasher);
            self.iteration_count.hash(&mut hasher);
            let new_seed = hasher.finish();
            self.seeds.push(new_seed);
            new_seed
        };

        self.iteration_count += 1;

        // Log which seed is being used for this iteration
        tracing::info!(
            "🌱 Starting iteration {} with seed {} (iteration {}/{})",
            self.iteration_count,
            seed,
            self.iteration_count,
            match &self.control {
                IterationControl::FixedCount(count) => *count,
                IterationControl::TimeLimit(_) => 0, // Unknown count for time-based
                IterationControl::UntilAllSometimesReached(limit) => *limit,
            }
        );

        seed
    }

    /// Get the current iteration count.
    fn current_iteration(&self) -> usize {
        self.iteration_count
    }

    /// Get all seeds used so far.
    fn seeds_used(&self) -> &[u64] {
        &self.seeds[..self.iteration_count]
    }

    /// Check if all sometimes_assert! assertions have been reached with at least one success.
    /// This simplified version checks that we have some assertion results and they have successes.
    fn all_sometimes_assertions_reached() -> bool {
        let results = get_assertion_results();

        // Must have at least one assertion
        if results.is_empty() {
            tracing::debug!("No assertions found yet");
            return false;
        }

        // Check if all executed assertions have at least one success
        for (name, stats) in &results {
            if stats.total_checks > 0 && stats.successes == 0 {
                tracing::debug!(
                    "Assertion '{}' executed {} times but never succeeded",
                    name,
                    stats.total_checks
                );
                return false;
            }
            tracing::debug!(
                "Assertion '{}' succeeded {} times out of {}",
                name,
                stats.successes,
                stats.total_checks
            );
        }

        tracing::debug!("All assertions have at least one success!");
        true
    }
}

/// Collects and aggregates metrics across simulation iterations.
struct MetricsCollector {
    successful_runs: usize,
    failed_runs: usize,
    aggregated_metrics: SimulationMetrics,
    individual_metrics: Vec<SimulationResult<SimulationMetrics>>,
    faulty_seeds: Vec<u64>,
}

impl MetricsCollector {
    /// Create a new metrics collector.
    fn new() -> Self {
        Self {
            successful_runs: 0,
            failed_runs: 0,
            aggregated_metrics: SimulationMetrics::default(),
            individual_metrics: Vec::new(),
            faulty_seeds: Vec::new(),
        }
    }

    /// Record the results of an iteration and update aggregated metrics.
    fn record_iteration(
        &mut self,
        seed: u64,
        wall_time: Duration,
        all_results: Vec<SimulationResult<SimulationMetrics>>,
        sim_metrics: SimulationMetrics,
    ) {
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
            self.successful_runs += 1;
            tracing::info!("✅ Iteration completed successfully with seed {}", seed);

            // Combine workload and simulation metrics for this iteration
            let mut combined_metrics = iteration_metrics;

            // For simulated time: prefer simulation world time when it's significantly larger,
            // as this represents actual time advancement in the simulation
            if sim_metrics.simulated_time > combined_metrics.simulated_time {
                combined_metrics.simulated_time = sim_metrics.simulated_time;
            }

            // For events processed: prefer workload metrics when available, as these represent
            // the meaningful work done by workloads. Only use simulation metrics if workloads
            // didn't report any events (i.e., they are 0).
            if combined_metrics.events_processed == 0 && sim_metrics.events_processed > 0 {
                combined_metrics.events_processed = sim_metrics.events_processed;
            }

            // Aggregate metrics across iterations
            self.aggregated_metrics.wall_time += wall_time;
            self.aggregated_metrics.simulated_time += combined_metrics.simulated_time;
            self.aggregated_metrics.events_processed += combined_metrics.events_processed;

            self.individual_metrics.push(Ok(combined_metrics));
        } else {
            self.failed_runs += 1;
            tracing::error!("❌ Iteration FAILED with seed {}", seed);
            self.individual_metrics
                .push(Err(crate::SimulationError::InvalidState(format!(
                    "One or more workloads failed (seed {})",
                    seed
                ))));
            self.faulty_seeds.push(seed);
        }
    }

    /// Add faulty seeds from external sources (e.g., deadlock detection).
    fn add_faulty_seeds(&mut self, mut seeds: Vec<u64>) {
        self.faulty_seeds.append(&mut seeds);
    }

    /// Increment failed runs count.
    fn add_failed_runs(&mut self, count: usize) {
        self.failed_runs += count;
    }

    /// Generate the final simulation report.
    fn generate_report(
        self,
        iteration_count: usize,
        seeds_used: Vec<u64>,
        assertion_results: HashMap<String, AssertionStats>,
        assertion_violations: Vec<String>,
    ) -> SimulationReport {
        SimulationReport {
            iterations: iteration_count,
            successful_runs: self.successful_runs,
            failed_runs: self.failed_runs,
            metrics: self.aggregated_metrics,
            individual_metrics: self.individual_metrics,
            seeds_used,
            seeds_failing: self.faulty_seeds,
            assertion_results,
            assertion_violations,
        }
    }

    /// Get current statistics for logging.
    fn current_stats(&self) -> (usize, usize) {
        (self.successful_runs, self.failed_runs)
    }
}

/// Factory for creating workload topology configurations.
struct TopologyFactory;

impl TopologyFactory {
    /// Create topology for a specific workload within a set of all workloads.
    fn create_topology(
        workload_ip: &str,
        all_workloads: &[Workload],
        shutdown_signal: tokio_util::sync::CancellationToken,
    ) -> WorkloadTopology {
        let peer_ips = all_workloads
            .iter()
            .filter(|w| w.ip_address != workload_ip)
            .map(|w| w.ip_address.clone())
            .collect();

        let peer_names = all_workloads
            .iter()
            .filter(|w| w.ip_address != workload_ip)
            .map(|w| w.name.clone())
            .collect();

        WorkloadTopology {
            my_ip: workload_ip.to_string(),
            peer_ips,
            peer_names,
            shutdown_signal,
        }
    }
}

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
    /// failed seeds
    pub seeds_failing: Vec<u64>,
    /// Aggregated assertion results across all iterations
    pub assertion_results: HashMap<String, AssertionStats>,
    /// Assertion validation violations (if any)
    pub assertion_violations: Vec<String>,
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

        if !self.seeds_failing.is_empty() {
            writeln!(f)?;
            writeln!(f, "Faulty seeds: {:?}", self.seeds_failing)?;
        }

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
    /// The names of all other peers in the simulation (parallel to peer_ips)
    pub peer_names: Vec<String>,
    /// Shutdown signal that gets triggered when the first workload exits with Ok
    pub shutdown_signal: tokio_util::sync::CancellationToken,
}

impl WorkloadTopology {
    /// Find the IP address of a peer by its workload name
    pub fn get_peer_by_name(&self, name: &str) -> Option<String> {
        self.peer_names
            .iter()
            .position(|peer_name| peer_name == name)
            .map(|index| self.peer_ips[index].clone())
    }

    /// Get all peers with a name prefix (useful for finding multiple clients, servers, etc.)
    pub fn get_peers_with_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        self.peer_names
            .iter()
            .zip(self.peer_ips.iter())
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, ip)| (name.clone(), ip.clone()))
            .collect()
    }
}

/// Type alias for workload function signature to reduce complexity.
type WorkloadFn = Box<
    dyn Fn(
        crate::random::sim::SimRandomProvider,
        crate::SimNetworkProvider,
        crate::SimTimeProvider,
        crate::task::tokio_provider::TokioTaskProvider,
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
    use_random_config: bool,
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
                crate::random::sim::SimRandomProvider,
                crate::SimNetworkProvider,
                crate::SimTimeProvider,
                crate::task::tokio_provider::TokioTaskProvider,
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
            reset_sim_rng();
            set_sim_seed(seed);

            // Initialize buggify system for this iteration
            // Use moderate probabilities: 50% activation rate, 25% firing rate
            buggify_init(0.5, 0.25);

            // Create fresh NetworkConfiguration for this iteration
            let network_config = if self.use_random_config {
                crate::NetworkConfiguration::random_for_seed()
            } else {
                crate::NetworkConfiguration::default()
            };

            // Create shared SimWorld for this iteration using fresh network config
            let sim = crate::SimWorld::new_with_network_config_and_seed(network_config, seed);
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
            )
            .await;

            let (all_results, sim_metrics) = match orchestration_result {
                Ok((results, metrics)) => (results, metrics),
                Err((faulty_seeds_from_deadlock, failed_count)) => {
                    // Handle deadlock case - merge with existing state and return early
                    metrics_collector.add_faulty_seeds(faulty_seeds_from_deadlock);
                    metrics_collector.add_failed_runs(failed_count);

                    // Create early exit report
                    let assertion_results = get_assertion_results();
                    let assertion_violations = validate_assertion_contracts();
                    buggify_reset();

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
            buggify_reset();
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
        let assertion_results = get_assertion_results();
        let assertion_violations = validate_assertion_contracts();

        // Final buggify reset to ensure no impact on subsequent code
        buggify_reset();

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
    use crate::random::RandomProvider;
    use std::time::Duration;

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
