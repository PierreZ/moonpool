//! Workload orchestration and iteration management.
//!
//! This module provides utilities for orchestrating workload execution
//! and managing simulation iterations.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::chaos::invariant_trait::Invariant;
use crate::chaos::state_handle::StateHandle;
use crate::runner::builder::WorkloadClientInfo;
use crate::runner::context::SimContext;
use crate::runner::fault_injector::{FaultContext, FaultInjector, PhaseConfig};
use crate::runner::process::Process;
use crate::runner::tags::{ProcessTags, TagRegistry};
use crate::runner::topology::TopologyFactory;
use crate::runner::workload::Workload;
use crate::{SimulationResult, chaos::AssertionStats};

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

    /// Reset the no-progress counter (e.g. after triggering shutdown to give tasks a chance).
    pub(crate) fn reset(&mut self) {
        self.no_progress_count = 0;
    }
}

/// Configuration for server processes in the simulation.
///
/// Created by the builder after resolving process count and tags.
pub(crate) struct ProcessConfig<'a> {
    /// Factory for creating process instances.
    pub(crate) factory: &'a dyn Fn() -> Box<dyn Process>,
    /// Process (name, ip) pairs for topology.
    pub(crate) info: Vec<(String, String)>,
    /// Process IP addresses.
    pub(crate) ips: Vec<String>,
    /// Tag registry mapping process IPs to their resolved tags.
    pub(crate) tag_registry: TagRegistry,
}

/// Manages process lifecycle during a simulation run.
///
/// Tracks running process tasks and handles restarts when `ProcessRestart`
/// events fire in the simulation event queue.
struct ProcessManager<'a> {
    factory: Option<&'a dyn Fn() -> Box<dyn Process>>,
    handles: Vec<Option<tokio::task::JoinHandle<()>>>,
    /// Per-process shutdown tokens (child of the global shutdown_signal).
    /// Cancelling a child signals only that process; cancelling the parent
    /// signals all processes.
    process_tokens: Vec<Option<tokio_util::sync::CancellationToken>>,
    ips: Vec<String>,
    tag_registry: TagRegistry,
    all_entities: Vec<(String, String)>,
    /// Count of currently dead (killed but not yet restarted) processes.
    dead_count: Rc<Cell<usize>>,
}

impl<'a> ProcessManager<'a> {
    /// Create an empty process manager (no processes configured).
    fn new_empty() -> Self {
        Self {
            factory: None,
            handles: Vec::new(),
            process_tokens: Vec::new(),
            ips: Vec::new(),
            tag_registry: TagRegistry::default(),
            all_entities: Vec::new(),
            dead_count: Rc::new(Cell::new(0)),
        }
    }

    /// Create a process manager from config and booted process handles.
    fn new(
        factory: &'a dyn Fn() -> Box<dyn Process>,
        handles: Vec<Option<tokio::task::JoinHandle<()>>>,
        process_tokens: Vec<Option<tokio_util::sync::CancellationToken>>,
        ips: Vec<String>,
        tag_registry: TagRegistry,
        all_entities: Vec<(String, String)>,
    ) -> Self {
        Self {
            factory: Some(factory),
            handles,
            process_tokens,
            ips,
            tag_registry,
            all_entities,
            dead_count: Rc::new(Cell::new(0)),
        }
    }

    /// Get a shared reference to the dead process counter.
    fn dead_count(&self) -> Rc<Cell<usize>> {
        self.dead_count.clone()
    }

    /// Resolve process index from IP, returning None for unknown IPs.
    fn index_for_ip(&self, ip: std::net::IpAddr) -> Option<usize> {
        let ip_str = ip.to_string();
        self.ips.iter().position(|p| p == &ip_str)
    }

    /// Signal a graceful shutdown for a process by cancelling its per-process token.
    ///
    /// The process will see `ctx.shutdown().is_cancelled()` and can perform
    /// cleanup before the force-kill timer fires.
    fn signal_graceful_shutdown(&mut self, ip: std::net::IpAddr) {
        let Some(idx) = self.index_for_ip(ip) else {
            tracing::warn!("ProcessGracefulShutdown for unknown IP {}", ip);
            return;
        };
        if let Some(token) = &self.process_tokens[idx] {
            token.cancel();
            self.dead_count.set(self.dead_count.get() + 1);
            tracing::info!(
                "Signaled graceful shutdown for process at {} (index {})",
                ip,
                idx
            );
        }
    }

    /// Abort a specific process task (force-kill after grace period).
    fn abort_process(&mut self, ip: std::net::IpAddr) {
        let Some(idx) = self.index_for_ip(ip) else {
            tracing::warn!("ProcessForceKill for unknown IP {}", ip);
            return;
        };
        if let Some(handle) = self.handles[idx].take() {
            handle.abort();
            tracing::info!("Force-killed process at {} (index {})", ip, idx);
        }
        // Clear the token — a new one will be created on restart
        self.process_tokens[idx] = None;
    }

    /// Handle a ProcessRestart event by spawning a new process task.
    fn handle_restart(
        &mut self,
        ip: std::net::IpAddr,
        providers: &crate::SimProviders,
        state: &StateHandle,
        shutdown_signal: &tokio_util::sync::CancellationToken,
    ) {
        let ip_str = ip.to_string();
        let Some(idx) = self.index_for_ip(ip) else {
            tracing::warn!("ProcessRestart for unknown IP {}", ip);
            return;
        };
        let Some(factory) = self.factory else {
            tracing::warn!("ProcessRestart but no process factory configured");
            return;
        };

        // Abort old task if still running (safety net)
        if let Some(handle) = self.handles[idx].take() {
            handle.abort();
        }

        // Create fresh per-process token as child of global shutdown
        let process_token = shutdown_signal.child_token();
        self.process_tokens[idx] = Some(process_token.clone());

        // Create fresh process instance
        let mut process = factory();
        let process_tags = self.tag_registry.tags_for(ip).cloned().unwrap_or_default();
        let topology = TopologyFactory::create_topology_with_processes(
            &ip_str,
            idx,
            self.ips.len(),
            &self.all_entities,
            &self.ips,
            process_tags,
            self.tag_registry.clone(),
            process_token,
        );
        let ctx = SimContext::new(providers.clone(), topology, state.clone());
        let ip_for_log = ip_str.clone();
        let handle = tokio::task::spawn_local(async move {
            if let Err(e) = process.run(&ctx).await {
                tracing::debug!("Restarted process at {} exited: {}", ip_for_log, e);
            }
        });
        self.handles[idx] = Some(handle);
        // Process is alive again
        let current = self.dead_count.get();
        if current > 0 {
            self.dead_count.set(current - 1);
        }
        tracing::info!("Process at {} restarted (index {})", ip_str, idx);
    }

    /// Abort all running process tasks.
    fn abort_all(&mut self) {
        for handle_opt in self.handles.iter_mut() {
            if let Some(handle) = handle_opt.take() {
                handle.abort();
            }
        }
    }
}

/// Orchestrates workload execution and event processing.
pub(crate) struct WorkloadOrchestrator;

/// Result of a completed workload task.
type WorkloadResult = (Box<dyn Workload>, SimulationResult<()>);

/// Result of a completed fault injector task.
type InjectorResult = (Box<dyn FaultInjector>, SimulationResult<()>);

impl WorkloadOrchestrator {
    /// Execute all workloads using the new lifecycle: setup → run → check.
    ///
    /// Returns workloads and fault injectors back to the caller for reuse across iterations.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub(crate) async fn orchestrate_workloads(
        mut workloads: Vec<Box<dyn Workload>>,
        fault_injectors: Vec<Box<dyn FaultInjector>>,
        invariants: &[Box<dyn Invariant>],
        workload_info: &[(String, String)],
        client_info: &[WorkloadClientInfo],
        process_config: Option<ProcessConfig<'_>>,
        seed: u64,
        mut sim: crate::sim::SimWorld,
        phase_config: Option<&PhaseConfig>,
        iteration_count: usize,
    ) -> Result<
        (
            Vec<Box<dyn Workload>>,
            Vec<Box<dyn FaultInjector>>,
            Vec<SimulationResult<()>>,
            SimulationMetrics,
        ),
        (Vec<u64>, usize),
    > {
        tracing::debug!(
            "Orchestrating {} workload(s), {} fault injector(s), {} process(es)",
            workloads.len(),
            fault_injectors.len(),
            process_config.as_ref().map_or(0, |pc| pc.ips.len()),
        );

        // Extract process info (cloned for use in setup/check phases)
        let process_ips: Vec<String> = process_config
            .as_ref()
            .map(|pc| pc.ips.clone())
            .unwrap_or_default();
        let tag_registry: TagRegistry = process_config
            .as_ref()
            .map(|pc| pc.tag_registry.clone())
            .unwrap_or_default();
        let process_info: Vec<(String, String)> = process_config
            .as_ref()
            .map(|pc| pc.info.clone())
            .unwrap_or_default();

        // Build combined entity list (workloads + processes) for topology
        let all_entities: Vec<(String, String)> = workload_info
            .iter()
            .chain(process_info.iter())
            .cloned()
            .collect();

        // Create shared state for cross-workload communication and invariant checking
        let state = StateHandle::new();

        // Create workload shutdown signal
        let shutdown_signal = tokio_util::sync::CancellationToken::new();

        // Create SimProviders (Clone-able, shared across workload contexts)
        let providers = crate::SimProviders::new(sim.downgrade(), seed);

        // === BOOT PROCESSES ===
        let mut process_handles: Vec<Option<tokio::task::JoinHandle<()>>> = Vec::new();
        let mut process_tokens: Vec<Option<tokio_util::sync::CancellationToken>> = Vec::new();
        if let Some(ref pc) = process_config {
            for (i, ip) in pc.ips.iter().enumerate() {
                let mut process = (pc.factory)();
                let ip_addr: std::net::IpAddr = ip.parse().map_err(|_| (vec![seed], 1usize))?;
                let process_tags = pc
                    .tag_registry
                    .tags_for(ip_addr)
                    .cloned()
                    .unwrap_or_default();
                // Per-process token: child of global shutdown_signal
                let process_token = shutdown_signal.child_token();
                let topology = TopologyFactory::create_topology_with_processes(
                    ip,
                    i,
                    pc.ips.len(),
                    &all_entities,
                    &pc.ips,
                    process_tags,
                    pc.tag_registry.clone(),
                    process_token.clone(),
                );
                let ctx = SimContext::new(providers.clone(), topology, state.clone());
                let ip_for_log = ip.clone();
                let handle = tokio::task::spawn_local(async move {
                    if let Err(e) = process.run(&ctx).await {
                        tracing::debug!("Process at {} exited: {}", ip_for_log, e);
                    }
                });
                process_handles.push(Some(handle));
                process_tokens.push(Some(process_token));
                tracing::debug!("Booted process {} at {}", i, ip);
            }
        }

        // Build process manager for lifecycle management (consumes process_config)
        let mut process_manager = match process_config {
            Some(pc) => ProcessManager::new(
                pc.factory,
                process_handles,
                process_tokens,
                pc.ips,
                pc.tag_registry,
                all_entities.clone(),
            ),
            None => ProcessManager::new_empty(),
        };

        // === SETUP PHASE ===
        let mut contexts = Vec::with_capacity(workloads.len());
        for (i, (_, ip)) in workload_info.iter().enumerate() {
            let WorkloadClientInfo {
                client_id,
                client_count,
            } = client_info[i];
            let topology = TopologyFactory::create_topology_with_processes(
                ip,
                client_id,
                client_count,
                &all_entities,
                &process_ips,
                ProcessTags::default(),
                tag_registry.clone(),
                shutdown_signal.clone(),
            );
            let ctx = SimContext::new(providers.clone(), topology, state.clone());
            contexts.push(ctx);
        }

        for (workload, ctx) in workloads.iter_mut().zip(contexts.iter()) {
            tracing::debug!("Setting up workload: {}", workload.name());
            if let Err(e) = workload.setup(ctx).await {
                tracing::error!("Workload '{}' setup failed: {}", workload.name(), e);
                let mut results: Vec<SimulationResult<()>> = vec![Err(e)];
                for _ in 1..workloads.len() {
                    results.push(Ok(()));
                }
                process_manager.abort_all();
                let sim_metrics = sim.extract_metrics();
                return Ok((workloads, fault_injectors, results, sim_metrics));
            }
        }

        // === RUN PHASE ===
        let (workloads, fault_injectors, results) = if let Some(pc) = phase_config {
            Self::run_with_phases(
                workloads,
                fault_injectors,
                invariants,
                &all_entities,
                contexts,
                &state,
                seed,
                &mut sim,
                pc,
                iteration_count,
                &shutdown_signal,
                &providers,
                &mut process_manager,
            )
            .await?
        } else {
            Self::run_without_phases(
                workloads,
                fault_injectors,
                invariants,
                contexts,
                &state,
                seed,
                &mut sim,
                iteration_count,
                &shutdown_signal,
                &providers,
                &mut process_manager,
            )
            .await?
        };

        // Kill all remaining processes
        process_manager.abort_all();

        // Drain remaining events
        sim.run_until_empty();

        // === CHECK PHASE ===
        let mut check_contexts = Vec::with_capacity(workload_info.len());
        for (i, (_, ip)) in workload_info.iter().enumerate() {
            let WorkloadClientInfo {
                client_id,
                client_count,
            } = client_info[i];
            let topology = TopologyFactory::create_topology_with_processes(
                ip,
                client_id,
                client_count,
                &all_entities,
                &process_ips,
                ProcessTags::default(),
                tag_registry.clone(),
                shutdown_signal.clone(),
            );
            let ctx = SimContext::new(providers.clone(), topology, state.clone());
            check_contexts.push(ctx);
        }

        let mut returned_workloads = workloads;
        for (workload, ctx) in returned_workloads.iter_mut().zip(check_contexts.iter()) {
            tracing::debug!("Running check for workload: {}", workload.name());
            if let Err(e) = workload.check(ctx).await {
                tracing::error!("Workload '{}' check failed: {}", workload.name(), e);
            }
        }

        // Extract final simulation metrics
        let sim_metrics = sim.extract_metrics();

        // If this is a forked child, exit immediately to return control to parent.
        if moonpool_explorer::explorer_is_child() {
            let code =
                if results.iter().all(|r| r.is_ok()) && !crate::chaos::has_always_violations() {
                    0
                } else {
                    42
                };
            moonpool_explorer::exit_child(code);
        }

        Ok((returned_workloads, fault_injectors, results, sim_metrics))
    }

    /// Run workloads without phase config (first completion triggers shutdown).
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    async fn run_without_phases(
        workloads: Vec<Box<dyn Workload>>,
        fault_injectors: Vec<Box<dyn FaultInjector>>,
        invariants: &[Box<dyn Invariant>],
        contexts: Vec<SimContext>,
        state: &StateHandle,
        seed: u64,
        sim: &mut crate::sim::SimWorld,
        iteration_count: usize,
        shutdown_signal: &tokio_util::sync::CancellationToken,
        providers: &crate::SimProviders,
        process_manager: &mut ProcessManager<'_>,
    ) -> Result<
        (
            Vec<Box<dyn Workload>>,
            Vec<Box<dyn FaultInjector>>,
            Vec<SimulationResult<()>>,
        ),
        (Vec<u64>, usize),
    > {
        tracing::debug!("Spawning {} workload(s) (no phase config)", workloads.len());

        // Spawn all workload runs
        let total = workloads.len();
        let mut handles: Vec<Option<tokio::task::JoinHandle<WorkloadResult>>> =
            Vec::with_capacity(total);
        for (workload, ctx) in workloads.into_iter().zip(contexts.into_iter()) {
            let handle = tokio::task::spawn_local(async move {
                let mut w = workload;
                let result = w.run(&ctx).await;
                (w, result)
            });
            handles.push(Some(handle));
        }

        let mut collected: Vec<Option<WorkloadResult>> = (0..total).map(|_| None).collect();
        let mut loop_count: u64 = 0;
        let mut deadlock_detector = DeadlockDetector::new(3);
        let mut shutdown_triggered = false;

        loop {
            let active_count = handles.iter().filter(|h| h.is_some()).count();
            if active_count == 0 {
                break;
            }

            loop_count += 1;
            if loop_count.is_multiple_of(100) {
                tracing::debug!(
                    "Cooperative loop iteration {}, {} handles active, {} pending events",
                    loop_count,
                    active_count,
                    sim.pending_event_count()
                );
            }

            let initial_handle_count = active_count;
            let initial_event_count = sim.pending_event_count();

            // Process one simulation event
            if sim.pending_event_count() > 0 {
                sim.step();
                let current_time_ms = sim.current_time().as_millis() as u64;
                Self::check_invariants(state, current_time_ms, invariants);

                // Handle process restarts
                // Handle process lifecycle events
                match sim.last_processed_event() {
                    Some(crate::sim::Event::ProcessGracefulShutdown {
                        ip,
                        grace_period_ms,
                        recovery_delay_ms,
                    }) => {
                        process_manager.signal_graceful_shutdown(ip);
                        sim.schedule_event(
                            crate::sim::Event::ProcessForceKill {
                                ip,
                                recovery_delay_ms,
                            },
                            Duration::from_millis(grace_period_ms),
                        );
                    }
                    Some(crate::sim::Event::ProcessForceKill {
                        ip,
                        recovery_delay_ms,
                    }) => {
                        process_manager.abort_process(ip);
                        sim.abort_all_connections_for_ip(ip);
                        sim.schedule_process_restart(ip, Duration::from_millis(recovery_delay_ms));
                    }
                    Some(crate::sim::Event::ProcessRestart { ip }) => {
                        process_manager.handle_restart(ip, providers, state, shutdown_signal);
                    }
                    _ => {}
                }
            }

            // Collect finished handles
            let mut any_finished = false;
            for i in 0..handles.len() {
                let finished = handles[i].as_ref().is_some_and(|h| h.is_finished());
                if finished {
                    let handle = handles[i].take().expect("just checked Some");
                    match handle.await {
                        Ok((workload, result)) => {
                            tracing::debug!("Workload '{}' completed", workload.name());
                            collected[i] = Some((workload, result));
                        }
                        Err(_) => {
                            tracing::error!("Workload task panicked");
                        }
                    }
                    any_finished = true;
                }
            }

            // Trigger shutdown on first completion
            if any_finished && !shutdown_triggered {
                Self::trigger_shutdown(sim, shutdown_signal);
                shutdown_triggered = true;
            }

            let current_active = handles.iter().filter(|h| h.is_some()).count();

            // Deadlock detection: trigger shutdown first to give tasks a chance to exit,
            // only fail on the second detection (genuine deadlock).
            if deadlock_detector.check_deadlock(
                current_active,
                initial_handle_count,
                sim.pending_event_count(),
                initial_event_count,
            ) {
                if !shutdown_triggered {
                    tracing::warn!(
                        "No progress detected on iteration {} with seed {}: {} tasks remaining. Triggering shutdown to unblock workloads.",
                        iteration_count,
                        seed,
                        current_active,
                    );
                    Self::trigger_shutdown(sim, shutdown_signal);
                    shutdown_triggered = true;
                    deadlock_detector.reset();
                } else {
                    tracing::error!(
                        "DEADLOCK detected on iteration {} with seed {}: {} tasks remaining after {} no-progress iterations",
                        iteration_count,
                        seed,
                        current_active,
                        deadlock_detector.no_progress_count()
                    );
                    return Err((vec![seed], 1));
                }
            }

            // Yield to allow tasks to make progress
            if current_active > 0 {
                tokio::task::yield_now().await;
            }
        }

        // Build return values
        let mut returned_workloads = Vec::with_capacity(total);
        let mut results = Vec::with_capacity(total);

        for item in collected {
            match item {
                Some((workload, result)) => {
                    returned_workloads.push(workload);
                    results.push(result);
                }
                None => {
                    results.push(Err(crate::SimulationError::InvalidState(
                        "Task panicked".to_string(),
                    )));
                }
            }
        }

        Ok((returned_workloads, fault_injectors, results))
    }

    /// Run workloads with two-phase chaos/recovery lifecycle.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    async fn run_with_phases(
        workloads: Vec<Box<dyn Workload>>,
        fault_injectors: Vec<Box<dyn FaultInjector>>,
        invariants: &[Box<dyn Invariant>],
        all_entities: &[(String, String)],
        contexts: Vec<SimContext>,
        state: &StateHandle,
        seed: u64,
        sim: &mut crate::sim::SimWorld,
        phase_config: &PhaseConfig,
        iteration_count: usize,
        shutdown_signal: &tokio_util::sync::CancellationToken,
        providers: &crate::SimProviders,
        process_manager: &mut ProcessManager<'_>,
    ) -> Result<
        (
            Vec<Box<dyn Workload>>,
            Vec<Box<dyn FaultInjector>>,
            Vec<SimulationResult<()>>,
        ),
        (Vec<u64>, usize),
    > {
        tracing::debug!(
            "Running with phases: chaos={:?}, recovery={:?}",
            phase_config.chaos_duration,
            phase_config.recovery_duration
        );

        let chaos_shutdown = tokio_util::sync::CancellationToken::new();
        let all_ips: Vec<String> = all_entities.iter().map(|(_, ip)| ip.clone()).collect();

        // Spawn all workload runs
        let total_workloads = workloads.len();
        let mut workload_handles: Vec<Option<tokio::task::JoinHandle<WorkloadResult>>> =
            Vec::with_capacity(total_workloads);
        for (workload, ctx) in workloads.into_iter().zip(contexts.into_iter()) {
            let handle = tokio::task::spawn_local(async move {
                let mut w = workload;
                let result = w.run(&ctx).await;
                (w, result)
            });
            workload_handles.push(Some(handle));
        }

        // Spawn all fault injectors
        let total_injectors = fault_injectors.len();
        let mut injector_handles: Vec<Option<tokio::task::JoinHandle<InjectorResult>>> =
            Vec::with_capacity(total_injectors);
        for fi in fault_injectors.into_iter() {
            // Create a SimWorld handle for the fault context (Rc clone via downgrade/upgrade)
            let fault_sim = sim
                .downgrade()
                .upgrade()
                .map_err(|_| (vec![seed], 1usize))?;
            let fault_ctx = FaultContext::new(
                fault_sim,
                all_ips.clone(),
                crate::runner::fault_injector::ProcessInfo {
                    process_ips: process_manager.ips.clone(),
                    tag_registry: process_manager.tag_registry.clone(),
                    dead_count: process_manager.dead_count(),
                },
                crate::SimRandomProvider::new(seed),
                sim.time_provider(),
                chaos_shutdown.clone(),
            );
            let handle = tokio::task::spawn_local(async move {
                let mut injector = fi;
                let result = injector.inject(&fault_ctx).await;
                (injector, result)
            });
            injector_handles.push(Some(handle));
        }

        // Event loop
        let chaos_start = sim.current_time();
        let mut chaos_ended = false;
        let mut recovery_ended = false;
        let mut deadlock_detector = DeadlockDetector::new(3);
        let mut shutdown_triggered = false;

        let mut workload_collected: Vec<Option<WorkloadResult>> =
            (0..total_workloads).map(|_| None).collect();

        loop {
            let active_workloads = workload_handles.iter().filter(|h| h.is_some()).count();
            if active_workloads == 0 {
                break;
            }

            let elapsed = sim.current_time().saturating_sub(chaos_start);
            let initial_handle_count = active_workloads;
            let initial_event_count = sim.pending_event_count();

            // Phase transitions
            if !chaos_ended && elapsed >= phase_config.chaos_duration {
                tracing::debug!("Chaos phase ended, transitioning to recovery");
                chaos_shutdown.cancel();
                Self::heal_all_partitions(sim, &all_ips);
                chaos_ended = true;
            }

            if !recovery_ended
                && chaos_ended
                && elapsed >= phase_config.chaos_duration + phase_config.recovery_duration
            {
                tracing::debug!("Recovery phase ended, triggering shutdown");
                Self::trigger_shutdown(sim, shutdown_signal);
                recovery_ended = true;
                shutdown_triggered = true;
            }

            // Process one simulation event
            if sim.pending_event_count() > 0 {
                sim.step();
                let current_time_ms = sim.current_time().as_millis() as u64;
                Self::check_invariants(state, current_time_ms, invariants);

                // Handle process restarts
                // Handle process lifecycle events
                match sim.last_processed_event() {
                    Some(crate::sim::Event::ProcessGracefulShutdown {
                        ip,
                        grace_period_ms,
                        recovery_delay_ms,
                    }) => {
                        process_manager.signal_graceful_shutdown(ip);
                        sim.schedule_event(
                            crate::sim::Event::ProcessForceKill {
                                ip,
                                recovery_delay_ms,
                            },
                            Duration::from_millis(grace_period_ms),
                        );
                    }
                    Some(crate::sim::Event::ProcessForceKill {
                        ip,
                        recovery_delay_ms,
                    }) => {
                        process_manager.abort_process(ip);
                        sim.abort_all_connections_for_ip(ip);
                        sim.schedule_process_restart(ip, Duration::from_millis(recovery_delay_ms));
                    }
                    Some(crate::sim::Event::ProcessRestart { ip }) => {
                        process_manager.handle_restart(ip, providers, state, shutdown_signal);
                    }
                    _ => {}
                }
            }

            // Collect finished workload handles
            for i in 0..workload_handles.len() {
                let finished = workload_handles[i]
                    .as_ref()
                    .is_some_and(|h| h.is_finished());
                if finished {
                    let handle = workload_handles[i].take().expect("just checked Some");
                    match handle.await {
                        Ok((workload, result)) => {
                            tracing::debug!("Workload '{}' completed", workload.name());
                            workload_collected[i] = Some((workload, result));
                        }
                        Err(_) => {
                            tracing::error!("Workload task panicked");
                        }
                    }
                }
            }

            // Collect finished injector handles
            for handle_opt in injector_handles.iter_mut() {
                let finished = handle_opt.as_ref().is_some_and(|h| h.is_finished());
                if finished {
                    let handle = handle_opt.take().expect("just checked Some");
                    match handle.await {
                        Ok((_injector, _result)) => {
                            tracing::debug!("Fault injector completed");
                        }
                        Err(_) => {
                            tracing::error!("Fault injector task panicked");
                        }
                    }
                }
            }

            let current_active = workload_handles.iter().filter(|h| h.is_some()).count();

            // Deadlock detection: trigger shutdown first to give tasks a chance to exit,
            // only fail on the second detection (genuine deadlock).
            if deadlock_detector.check_deadlock(
                current_active,
                initial_handle_count,
                sim.pending_event_count(),
                initial_event_count,
            ) {
                if !shutdown_triggered {
                    tracing::warn!(
                        "No progress detected on iteration {} with seed {}: {} tasks remaining. Triggering shutdown to unblock workloads.",
                        iteration_count,
                        seed,
                        current_active,
                    );
                    Self::trigger_shutdown(sim, shutdown_signal);
                    shutdown_triggered = true;
                    deadlock_detector.reset();
                } else {
                    tracing::error!(
                        "DEADLOCK detected on iteration {} with seed {}: {} tasks remaining",
                        iteration_count,
                        seed,
                        current_active
                    );
                    return Err((vec![seed], 1));
                }
            }

            // Yield to allow tasks to make progress
            if current_active > 0 {
                tokio::task::yield_now().await;
            }
        }

        // Collect returned fault injectors
        let mut returned_injectors = Vec::new();
        for handle_opt in injector_handles.iter_mut() {
            if let Some(handle) = handle_opt.take() {
                if handle.is_finished() {
                    if let Ok((injector, _)) = handle.await {
                        returned_injectors.push(injector);
                    }
                } else {
                    handle.abort();
                }
            }
        }

        // Build return values
        let mut returned_workloads = Vec::with_capacity(total_workloads);
        let mut results = Vec::with_capacity(total_workloads);

        for item in workload_collected {
            match item {
                Some((workload, result)) => {
                    returned_workloads.push(workload);
                    results.push(result);
                }
                None => {
                    results.push(Err(crate::SimulationError::InvalidState(
                        "Task panicked".to_string(),
                    )));
                }
            }
        }

        Ok((returned_workloads, returned_injectors, results))
    }

    /// Trigger shutdown signal and schedule wake events.
    fn trigger_shutdown(
        sim: &mut crate::sim::SimWorld,
        shutdown_signal: &tokio_util::sync::CancellationToken,
    ) {
        tracing::debug!("Triggering shutdown signal");
        shutdown_signal.cancel();

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
        if invariants.is_empty() {
            return;
        }

        for invariant in invariants {
            invariant.check(state, sim_time_ms);
        }
    }

    /// Heal all network partitions between all IP pairs.
    fn heal_all_partitions(sim: &mut crate::sim::SimWorld, all_ips: &[String]) {
        for i in 0..all_ips.len() {
            for j in (i + 1)..all_ips.len() {
                if let (Ok(a_ip), Ok(b_ip)) = (
                    all_ips[i].parse::<std::net::IpAddr>(),
                    all_ips[j].parse::<std::net::IpAddr>(),
                ) {
                    let _ = sim.restore_partition(a_ip, b_ip);
                }
            }
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
            "Starting iteration {} with seed {} (iteration {}/{})",
            self.iteration_count,
            seed,
            self.iteration_count,
            match &self.control {
                super::builder::IterationControl::FixedCount(count) => *count,
                super::builder::IterationControl::TimeLimit(_) => 0,
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

    /// Record the results of an iteration.
    ///
    /// An iteration is considered failed if any workload returned an error
    /// OR if assertion violations were detected during the iteration.
    pub(crate) fn record_iteration(
        &mut self,
        seed: u64,
        wall_time: Duration,
        all_results: &[SimulationResult<()>],
        has_assertion_violations: bool,
        sim_metrics: SimulationMetrics,
    ) {
        let workloads_ok = all_results.iter().all(|r| r.is_ok());
        let all_ok = workloads_ok && !has_assertion_violations;

        if all_ok {
            self.record_success(seed, wall_time, sim_metrics);
        } else {
            self.record_failure(seed);
        }
    }

    /// Record a successful iteration.
    fn record_success(&mut self, seed: u64, wall_time: Duration, sim_metrics: SimulationMetrics) {
        self.successful_runs += 1;
        tracing::info!("Iteration completed successfully with seed {}", seed);

        self.aggregated_metrics.wall_time += wall_time;
        self.aggregated_metrics.simulated_time += sim_metrics.simulated_time;
        self.aggregated_metrics.events_processed += sim_metrics.events_processed;

        let mut individual = sim_metrics;
        individual.wall_time = wall_time;
        self.individual_metrics.push(Ok(individual));
    }

    /// Record a failed iteration.
    fn record_failure(&mut self, seed: u64) {
        self.failed_runs += 1;
        tracing::error!("Iteration FAILED with seed {}", seed);
        self.individual_metrics
            .push(Err(crate::SimulationError::InvalidState(format!(
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn generate_report(
        self,
        iteration_count: usize,
        seeds_used: Vec<u64>,
        assertion_results: HashMap<String, AssertionStats>,
        assertion_violations: Vec<String>,
        coverage_violations: Vec<String>,
        exploration: Option<super::report::ExplorationReport>,
        assertion_details: Vec<super::report::AssertionDetail>,
        bucket_summaries: Vec<super::report::BucketSiteSummary>,
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
            coverage_violations,
            exploration,
            assertion_details,
            bucket_summaries,
        }
    }
}
