//! Workload orchestration and iteration management.
//!
//! This module provides utilities for orchestrating workload execution
//! and managing simulation iterations.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::{SimulationResult, chaos::AssertionStats};

use super::report::SimulationMetrics;
use super::topology::{TopologyFactory, Workload};

/// Deadlock detection utility to identify stuck simulations.
#[derive(Debug, Default)]
pub(crate) struct DeadlockDetector {
    no_progress_count: usize,
    threshold: usize,
}

impl DeadlockDetector {
    /// Create a new deadlock detector with a threshold for consecutive no-progress iterations.
    pub(crate) fn new(threshold: usize) -> Self {
        Self {
            no_progress_count: 0,
            threshold,
        }
    }

    /// Check if deadlock conditions are met and update internal state.
    /// Returns true if deadlock is detected.
    pub(crate) fn check_deadlock(
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
    pub(crate) fn no_progress_count(&self) -> usize {
        self.no_progress_count
    }
}

/// Orchestrates workload execution and event processing.
pub(crate) struct WorkloadOrchestrator;

impl WorkloadOrchestrator {
    /// Execute all workloads using spawn_local and coordinate their execution.
    pub(crate) async fn orchestrate_workloads(
        workloads: &[Workload],
        seed: u64,
        provider: crate::SimNetworkProvider,
        mut sim: crate::sim::SimWorld,
        shutdown_signal: tokio_util::sync::CancellationToken,
        iteration_count: usize,
        invariants: &[crate::InvariantCheck],
    ) -> Result<(Vec<SimulationResult<SimulationMetrics>>, SimulationMetrics), (Vec<u64>, usize)>
    {
        tracing::debug!("Spawning {} workload(s) with spawn_local", workloads.len());

        // Create shared state registry for invariant checking
        let state_registry = crate::StateRegistry::new();

        let mut handles = Vec::new();
        for (idx, workload) in workloads.iter().enumerate() {
            tracing::debug!("Spawning workload {}: {}", idx, workload.name);

            let topology = TopologyFactory::create_topology(
                &workload.ip_address,
                workloads,
                shutdown_signal.clone(),
                state_registry.clone(),
            );

            let time_provider = sim.time_provider();
            let task_provider = sim.task_provider();
            // Create random provider from seed
            let random_provider = crate::random::SimRandomProvider::new(seed);
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

                // Check invariants after processing event
                let current_time_ms = sim.current_time().as_millis() as u64;
                Self::check_invariants(&state_registry, current_time_ms, invariants);
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
        sim: &mut crate::sim::SimWorld,
        shutdown_signal: &tokio_util::sync::CancellationToken,
    ) {
        tracing::debug!("First workload completed successfully, triggering shutdown signal");
        shutdown_signal.cancel();

        // Schedule a shutdown event to wake all tasks
        tracing::debug!("Scheduling Shutdown event to wake all tasks");
        sim.schedule_event(crate::sim::Event::Shutdown, Duration::from_nanos(1));

        // Schedule many periodic wake events to ensure tasks can check shutdown
        for i in 1..100 {
            sim.schedule_event(
                crate::sim::Event::Timer {
                    task_id: u64::MAX - i,
                },
                Duration::from_nanos(i),
            );
        }
    }

    /// Check all registered invariants against current actor states.
    ///
    /// If any invariant check panics, the simulation will immediately terminate with that panic.
    /// This implements the "crash early" philosophy inspired by FoundationDB.
    fn check_invariants(
        state_registry: &crate::StateRegistry,
        sim_time_ms: u64,
        invariants: &[crate::InvariantCheck],
    ) {
        if invariants.is_empty() {
            return;
        }

        let all_states = state_registry.get_all_states();
        for invariant in invariants {
            invariant(&all_states, sim_time_ms);
        }
    }
}

/// Manages iteration control, seed generation, and progress tracking.
pub(crate) struct IterationManager {
    control: super::builder::IterationControl,
    seeds: Vec<u64>,
    base_seed: u64,
    iteration_count: usize,
    start_time: Instant,
}

impl IterationManager {
    /// Create a new iteration manager with the given control strategy and initial seeds.
    pub(crate) fn new(control: super::builder::IterationControl, initial_seeds: Vec<u64>) -> Self {
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
    pub(crate) fn should_continue(&self) -> bool {
        match &self.control {
            super::builder::IterationControl::FixedCount(count) => self.iteration_count < *count,
            super::builder::IterationControl::TimeLimit(duration) => {
                self.start_time.elapsed() < *duration
            }
            super::builder::IterationControl::UntilAllSometimesReached(safety_limit) => {
                self.iteration_count < *safety_limit
                    && !(self.iteration_count > 0 && Self::all_sometimes_assertions_reached())
            }
        }
    }

    /// Get the seed for the current iteration and advance to the next.
    pub(crate) fn next_iteration(&mut self) -> u64 {
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
                super::builder::IterationControl::FixedCount(count) => *count,
                super::builder::IterationControl::TimeLimit(_) => 0, // Unknown count for time-based
                super::builder::IterationControl::UntilAllSometimesReached(limit) => *limit,
            }
        );

        seed
    }

    /// Get the current iteration count.
    pub(crate) fn current_iteration(&self) -> usize {
        self.iteration_count
    }

    /// Get all seeds used so far.
    pub(crate) fn seeds_used(&self) -> &[u64] {
        &self.seeds[..self.iteration_count]
    }

    /// Check if all sometimes_assert! assertions have been reached with at least one success.
    /// This simplified version checks that we have some assertion results and they have successes.
    fn all_sometimes_assertions_reached() -> bool {
        let results = crate::chaos::get_assertion_results();

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
pub(crate) struct MetricsCollector {
    successful_runs: usize,
    failed_runs: usize,
    aggregated_metrics: SimulationMetrics,
    individual_metrics: Vec<SimulationResult<SimulationMetrics>>,
    faulty_seeds: Vec<u64>,
}

impl MetricsCollector {
    /// Create a new metrics collector.
    pub(crate) fn new() -> Self {
        Self {
            successful_runs: 0,
            failed_runs: 0,
            aggregated_metrics: SimulationMetrics::default(),
            individual_metrics: Vec::new(),
            faulty_seeds: Vec::new(),
        }
    }

    /// Record the results of an iteration and update aggregated metrics.
    pub(crate) fn record_iteration(
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
    pub(crate) fn add_faulty_seeds(&mut self, mut seeds: Vec<u64>) {
        self.faulty_seeds.append(&mut seeds);
    }

    /// Increment failed runs count.
    pub(crate) fn add_failed_runs(&mut self, count: usize) {
        self.failed_runs += count;
    }

    /// Generate the final simulation report.
    pub(crate) fn generate_report(
        self,
        iteration_count: usize,
        seeds_used: Vec<u64>,
        assertion_results: HashMap<String, AssertionStats>,
        assertion_violations: Vec<String>,
    ) -> super::report::SimulationReport {
        super::report::SimulationReport {
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
    pub(crate) fn current_stats(&self) -> (usize, usize) {
        (self.successful_runs, self.failed_runs)
    }
}
