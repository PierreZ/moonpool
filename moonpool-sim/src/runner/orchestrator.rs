//! Workload orchestration and iteration management.
//!
//! This module provides utilities for orchestrating workload execution
//! and managing simulation iterations.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::chaos::state_handle::StateHandle;
use crate::chaos::{AssertionStats, Invariant};
use crate::runner::context::{CancellationToken, SimContext};
use crate::runner::fault_injector::{FaultInjector, PhaseConfig};
use crate::runner::workload::Workload;
use crate::{SimulationError, SimulationResult};

use super::report::SimulationMetrics;

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
    /// Execute workloads using the new setup/run/check lifecycle.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn orchestrate(
        workloads: &[Box<dyn Workload>],
        invariants: &[Box<dyn Invariant>],
        fault_injectors: &[Box<dyn FaultInjector>],
        phase_config: Option<&PhaseConfig>,
        workload_ips: &[String],
        seed: u64,
        mut sim: crate::sim::SimWorld,
        shutdown: CancellationToken,
        iteration_count: usize,
    ) -> Result<(Vec<SimulationResult<SimulationMetrics>>, SimulationMetrics), (Vec<u64>, usize)>
    {
        tracing::debug!(
            "Orchestrating {} workload(s), {} invariant(s), {} fault injector(s)",
            workloads.len(),
            invariants.len(),
            fault_injectors.len(),
        );

        // 1. Create shared StateHandle
        let state = StateHandle::new();

        // 2. Build peer lists and create SimContext per workload
        let mut contexts: Vec<SimContext> = Vec::new();
        for (i, _workload) in workloads.iter().enumerate() {
            let my_ip = workload_ips[i].clone();
            let peers: Vec<String> = workload_ips
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, ip)| ip.clone())
                .collect();

            let ctx = SimContext::new(
                sim.network_provider(),
                sim.time_provider(),
                crate::SimRandomProvider::new(seed),
                sim.storage_provider(),
                my_ip,
                peers,
                shutdown.clone(),
                state.clone(),
            );
            contexts.push(ctx);
        }

        // 3. Setup phase: call setup sequentially
        for (i, workload) in workloads.iter().enumerate() {
            tracing::debug!("Setup phase: {}", workload.name());
            if let Err(e) = workload.setup(&contexts[i]).await {
                tracing::error!("Setup failed for workload '{}': {}", workload.name(), e);
                return Err((vec![seed], 1));
            }
        }

        // 4. Run phase: spawn_local all workloads + fault injectors concurrently
        let mut handles = Vec::new();

        for (i, workload) in workloads.iter().enumerate() {
            tracing::debug!("Spawning workload: {}", workload.name());
            // Safety: ctx and workload outlive the spawned task because we
            // join all handles before returning from this function.
            let ctx_ptr = &contexts[i] as *const SimContext;
            let workload_ptr = &**workload as *const dyn Workload;
            let handle = tokio::task::spawn_local(async move {
                let ctx = unsafe { &*ctx_ptr };
                let w = unsafe { &*workload_ptr };
                w.run(ctx).await
            });
            handles.push(handle);
        }

        // Spawn fault injectors
        let mut fault_handles = Vec::new();
        if !fault_injectors.is_empty() {
            let fault_ctx = SimContext::new(
                sim.network_provider(),
                sim.time_provider(),
                crate::SimRandomProvider::new(seed.wrapping_add(1000)),
                sim.storage_provider(),
                workload_ips[0].clone(),
                workload_ips[1..].to_vec(),
                shutdown.clone(),
                state.clone(),
            );
            // Keep fault_ctx alive for the duration of the function
            let fault_ctx_box = Box::new(fault_ctx);
            let fault_ctx_ptr = &*fault_ctx_box as *const SimContext;

            for fi in fault_injectors.iter() {
                let fi_ptr = &**fi as *const dyn FaultInjector;
                let handle = tokio::task::spawn_local(async move {
                    let ctx = unsafe { &*fault_ctx_ptr };
                    let fi = unsafe { &*fi_ptr };
                    fi.inject(ctx).await
                });
                fault_handles.push(handle);
            }

            // Leak the box to keep it alive (will be cleaned up on process exit)
            // This is acceptable since simulations are short-lived.
            std::mem::forget(fault_ctx_box);
        }

        // 5. Event loop
        let mut results: Vec<SimulationResult<SimulationMetrics>> = Vec::new();
        let mut loop_count = 0;
        let mut deadlock_detector = DeadlockDetector::new(3);
        let mut shutdown_triggered = false;

        let phase_start = Instant::now();

        while !handles.is_empty() {
            loop_count += 1;
            if loop_count % 100 == 0 {
                tracing::debug!(
                    "Loop iteration {}, {} handles, {} events pending",
                    loop_count,
                    handles.len(),
                    sim.pending_event_count()
                );
            }

            let initial_handle_count = handles.len();
            let initial_event_count = sim.pending_event_count();

            // Process one simulation event
            if sim.pending_event_count() > 0 {
                sim.step();

                // Check invariants after event
                let current_time_ms = sim.current_time().as_millis() as u64;
                Self::check_invariants(&state, current_time_ms, invariants);
            }

            // Handle phase transitions
            if let Some(pc) = phase_config {
                let elapsed_ms = phase_start.elapsed().as_millis() as u64;
                if elapsed_ms > pc.chaos_duration_ms && !fault_handles.is_empty() {
                    tracing::debug!("Chaos phase complete, cancelling fault injectors");
                    for fh in fault_handles.drain(..) {
                        fh.abort();
                    }
                }
                if elapsed_ms > pc.chaos_duration_ms + pc.liveness_duration_ms
                    && !shutdown_triggered
                {
                    tracing::debug!("Liveness phase complete, triggering shutdown");
                    Self::trigger_shutdown(&mut sim, &shutdown);
                    shutdown_triggered = true;
                }
            }

            // Check completed handles
            let mut i = 0;
            while i < handles.len() {
                if handles[i].is_finished() {
                    let join_result = handles.remove(i).await;
                    let result: SimulationResult<SimulationMetrics> = match join_result {
                        Ok(Ok(())) => Ok(SimulationMetrics::default()),
                        Ok(Err(e)) => {
                            tracing::error!("Workload error: {}", e);
                            Err(e)
                        }
                        Err(_) => {
                            tracing::error!("Workload task panicked");
                            Err(SimulationError::InvalidState("Task panicked".to_string()))
                        }
                    };

                    if !shutdown_triggered && phase_config.is_none() {
                        Self::trigger_shutdown(&mut sim, &shutdown);
                        shutdown_triggered = true;
                    }

                    results.push(result);
                } else {
                    i += 1;
                }
            }

            // Deadlock detection
            if deadlock_detector.check_deadlock(
                handles.len(),
                initial_handle_count,
                sim.pending_event_count(),
                initial_event_count,
            ) {
                tracing::error!(
                    "DEADLOCK detected on iteration {} with seed {}: {} tasks remaining after {} iterations",
                    iteration_count,
                    seed,
                    handles.len(),
                    deadlock_detector.no_progress_count()
                );
                for _ in 0..handles.len() {
                    results.push(Err(SimulationError::InvalidState(format!(
                        "Deadlock detected on iteration {} with seed {}: tasks stuck with no events",
                        iteration_count, seed
                    ))));
                }
                return Err((vec![seed], 1));
            }

            // Yield to allow tasks to make progress
            if !handles.is_empty() {
                tokio::task::yield_now().await;
            }
        }

        // Cancel remaining fault injectors
        for fh in fault_handles {
            fh.abort();
        }

        tracing::debug!(
            "All workloads completed after {} loops, processing remaining events",
            loop_count
        );

        // Process remaining events
        sim.run_until_empty();

        // 6. Check phase: call check sequentially
        for (i, workload) in workloads.iter().enumerate() {
            tracing::debug!("Check phase: {}", workload.name());
            if let Err(e) = workload.check(&contexts[i]).await {
                tracing::error!("Check failed for workload '{}': {}", workload.name(), e);
                results.push(Err(e));
            }
        }

        // Extract final simulation metrics
        let sim_metrics = sim.extract_metrics();

        // 7. Explorer child exit
        if moonpool_explorer::explorer_is_child() {
            let code = if results.iter().all(|r| r.is_ok()) {
                0
            } else {
                42
            };
            moonpool_explorer::exit_child(code);
        }

        Ok((results, sim_metrics))
    }

    /// Trigger shutdown signal and schedule wake events.
    fn trigger_shutdown(sim: &mut crate::sim::SimWorld, shutdown: &CancellationToken) {
        tracing::debug!("Triggering shutdown signal");
        shutdown.cancel();

        sim.schedule_event(crate::sim::Event::Shutdown, Duration::from_nanos(1));
        for i in 1..100 {
            sim.schedule_event(
                crate::sim::Event::Timer {
                    task_id: u64::MAX - i,
                },
                Duration::from_nanos(i),
            );
        }
    }

    /// Check all registered invariants against current state.
    fn check_invariants(state: &StateHandle, sim_time_ms: u64, invariants: &[Box<dyn Invariant>]) {
        for invariant in invariants {
            invariant.check(state, sim_time_ms);
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

        tracing::info!(
            "Starting iteration {} with seed {} ({}/{})",
            self.iteration_count,
            seed,
            self.iteration_count,
            match &self.control {
                super::builder::IterationControl::FixedCount(count) => *count,
                super::builder::IterationControl::TimeLimit(_) => 0,
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

    /// Check if all sometimes assertions have been reached with at least one success.
    fn all_sometimes_assertions_reached() -> bool {
        let results = crate::chaos::get_assertion_results();

        if results.is_empty() {
            tracing::debug!("No assertions found yet");
            return false;
        }

        for (name, stats) in &results {
            if stats.total_checks > 0 && stats.successes == 0 {
                tracing::debug!(
                    "Assertion '{}' executed {} times but never succeeded",
                    name,
                    stats.total_checks
                );
                return false;
            }
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
        let (iteration_successful, workload_metrics) =
            Self::aggregate_workload_results(&all_results);

        if iteration_successful {
            self.record_success(seed, wall_time, workload_metrics, sim_metrics);
        } else {
            self.record_failure(seed);
        }
    }

    /// Aggregate metrics from all workload results.
    fn aggregate_workload_results(
        results: &[SimulationResult<SimulationMetrics>],
    ) -> (bool, SimulationMetrics) {
        let mut success = true;
        let mut metrics = SimulationMetrics::default();

        for result in results {
            match result {
                Ok(m) => {
                    metrics.simulated_time = metrics.simulated_time.max(m.simulated_time);
                    metrics.events_processed += m.events_processed;
                }
                Err(_) => success = false,
            }
        }

        (success, metrics)
    }

    /// Record a successful iteration.
    fn record_success(
        &mut self,
        seed: u64,
        wall_time: Duration,
        workload_metrics: SimulationMetrics,
        sim_metrics: SimulationMetrics,
    ) {
        self.successful_runs += 1;
        tracing::info!("Iteration completed successfully with seed {}", seed);

        let combined = Self::merge_metrics(workload_metrics, sim_metrics);

        self.aggregated_metrics.wall_time += wall_time;
        self.aggregated_metrics.simulated_time += combined.simulated_time;
        self.aggregated_metrics.events_processed += combined.events_processed;

        self.individual_metrics.push(Ok(combined));
    }

    /// Merge workload metrics with simulation metrics.
    fn merge_metrics(mut workload: SimulationMetrics, sim: SimulationMetrics) -> SimulationMetrics {
        if sim.simulated_time > workload.simulated_time {
            workload.simulated_time = sim.simulated_time;
        }
        if workload.events_processed == 0 {
            workload.events_processed = sim.events_processed;
        }
        workload
    }

    /// Record a failed iteration.
    fn record_failure(&mut self, seed: u64) {
        self.failed_runs += 1;
        tracing::error!("Iteration FAILED with seed {}", seed);
        self.individual_metrics
            .push(Err(SimulationError::InvalidState(format!(
                "One or more workloads failed (seed {})",
                seed
            ))));
        self.faulty_seeds.push(seed);
    }

    /// Add faulty seeds from external sources.
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
        exploration: Option<super::report::ExplorationReport>,
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
            exploration,
        }
    }

    /// Get current statistics for logging.
    pub(crate) fn current_stats(&self) -> (usize, usize) {
        (self.successful_runs, self.failed_runs)
    }
}
