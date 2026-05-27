//! Workload orchestration and iteration management.
//!
//! This module provides utilities for orchestrating workload execution
//! and managing simulation iterations.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tracing::field::valuable;

use crate::chaos::fault_events::{SIM_FAULT_TRAIL, SimFaultEvent};
use crate::chaos::state_handle::StateHandle;
use crate::observability::SimulationLayerHandle;
use crate::runner::builder::WorkloadClientInfo;
use crate::runner::context::SimContext;
use crate::runner::fault_injector::{FaultContext, FaultInjector};
use crate::runner::process::Process;
use crate::runner::tags::{ProcessTags, TagRegistry};
use crate::runner::topology::{TopologyFactory, TopologyInputs};
use crate::runner::workload::Workload;
use crate::{
    SimulationResult, assert_always_less_than_or_equal_to, assert_reachable, chaos::AssertionStats,
};

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
    /// Per-process shutdown tokens (child of the global `shutdown_signal`).
    /// Cancelling a child signals only that process; cancelling the parent
    /// signals all processes.
    process_tokens: Vec<Option<tokio_util::sync::CancellationToken>>,
    ips: Vec<String>,
    tag_registry: TagRegistry,
    all_entities: Vec<(String, String)>,
    /// Count of currently dead (killed but not yet restarted) processes.
    dead_count: Arc<AtomicUsize>,
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
            dead_count: Arc::new(AtomicUsize::new(0)),
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
            dead_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get a shared reference to the dead process counter.
    fn dead_count(&self) -> Arc<AtomicUsize> {
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
            self.dead_count.fetch_add(1, Ordering::Relaxed);
            assert_always_less_than_or_equal_to!(
                self.dead_count.load(Ordering::Relaxed),
                self.ips.len(),
                "dead_count <= process_count"
            );
            assert_reachable!("process_manager: graceful shutdown signaled");
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

    /// Handle a `ProcessRestart` event by spawning a new process task.
    fn handle_restart(
        &mut self,
        ip: std::net::IpAddr,
        sim: &crate::sim::WeakSimWorld,
        seed: u64,
        state: &StateHandle,
        obs: &SimulationLayerHandle,
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
        let topology = TopologyFactory::create_topology_with_processes(TopologyInputs {
            ip: &ip_str,
            client_id: idx,
            client_count: self.ips.len(),
            all_entities: &self.all_entities,
            process_ips: &self.ips,
            my_tags: process_tags,
            tag_registry: self.tag_registry.clone(),
            shutdown_signal: process_token,
        });
        let providers = crate::SimProviders::new(sim.clone(), seed, ip);
        let ctx = SimContext::new(providers, topology, state.clone(), obs.clone());
        let ip_for_log = ip_str.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = process.run(&ctx).await {
                tracing::debug!("Restarted process at {} exited: {}", ip_for_log, e);
            }
        });
        self.handles[idx] = Some(handle);
        // Process is alive again
        let current = self.dead_count.load(Ordering::Relaxed);
        if current > 0 {
            self.dead_count.fetch_sub(1, Ordering::Relaxed);
        }
        assert_reachable!("process_manager: process restarted");
        tracing::info!("Process at {} restarted (index {})", ip_str, idx);
    }

    /// Abort all running process tasks.
    fn abort_all(&mut self) {
        for handle_opt in &mut self.handles {
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

/// Result returned by a spawned `setup()` task: the workload, its context,
/// and the setup result.
type SetupTaskOutput = (Box<dyn Workload>, SimContext, SimulationResult<()>);

/// Handle to a spawned `setup()` task.
type SetupHandle = tokio::task::JoinHandle<SetupTaskOutput>;

/// Per-process join handles (in option slots so they can be drained).
type ProcessHandleSlots = Vec<Option<tokio::task::JoinHandle<()>>>;

/// Per-process cancellation tokens (in option slots so they can be drained).
type ProcessTokenSlots = Vec<Option<tokio_util::sync::CancellationToken>>;

/// Per-injector join handles (in option slots so they can be drained).
type InjectorHandleSlots = Vec<Option<tokio::task::JoinHandle<InjectorResult>>>;

/// Per-workload join handles (in option slots so they can be drained).
type WorkloadHandleSlots = Vec<Option<tokio::task::JoinHandle<WorkloadResult>>>;

/// Inputs needed to run the check phase.
struct CheckPhaseInputs<'a> {
    sim: &'a mut crate::sim::SimWorld,
    workloads: Vec<Box<dyn Workload>>,
    workload_info: &'a [(String, String)],
    client_info: &'a [WorkloadClientInfo],
    all_entities: &'a [(String, String)],
    process_ips: &'a [String],
    tag_registry: &'a TagRegistry,
    shutdown_signal: &'a tokio_util::sync::CancellationToken,
    seed: u64,
    state: &'a StateHandle,
    obs: &'a SimulationLayerHandle,
}

/// Shared inputs threaded through a phase that drives the cooperative loop.
struct PhaseEnv<'a, 'pm> {
    sim: &'a mut crate::sim::SimWorld,
    process_manager: &'a mut ProcessManager<'pm>,
    seed: u64,
    state: &'a StateHandle,
    obs: &'a SimulationLayerHandle,
    shutdown_signal: &'a tokio_util::sync::CancellationToken,
}

/// Aggregated borrows needed to drive the run phase.
struct RunPhaseInputs<'a, 'pm> {
    sim: &'a mut crate::sim::SimWorld,
    process_manager: &'a mut ProcessManager<'pm>,
    obs: &'a SimulationLayerHandle,
    state: &'a StateHandle,
    shutdown_signal: &'a tokio_util::sync::CancellationToken,
    chaos_shutdown: &'a tokio_util::sync::CancellationToken,
    chaos_duration: Option<Duration>,
    all_ips: &'a [String],
    workload_handles: &'a mut WorkloadHandleSlots,
    workload_collected: &'a mut [Option<WorkloadResult>],
    injector_handles: &'a mut InjectorHandleSlots,
    seed: u64,
    iteration_count: usize,
}

/// Aggregated borrows needed to build workload contexts.
struct WorkloadContextEnv<'a> {
    workload_info: &'a [(String, String)],
    client_info: &'a [WorkloadClientInfo],
    all_entities: &'a [(String, String)],
    process_ips: &'a [String],
    tag_registry: &'a TagRegistry,
    shutdown_signal: &'a tokio_util::sync::CancellationToken,
    sim: &'a crate::sim::SimWorld,
    seed: u64,
    state: &'a StateHandle,
    obs: &'a SimulationLayerHandle,
}

/// Result of a completed fault injector task.
type InjectorResult = (Box<dyn FaultInjector>, SimulationResult<()>);

/// Inputs to [`WorkloadOrchestrator::orchestrate_workloads`].
pub(crate) struct OrchestrateInputs<'a> {
    /// Workloads to drive through setup/run/check.
    pub(crate) workloads: Vec<Box<dyn Workload>>,
    /// Fault injectors to spawn during the chaos phase.
    pub(crate) fault_injectors: Vec<Box<dyn FaultInjector>>,
    /// Shared observability handle for the simulation.
    pub(crate) obs: SimulationLayerHandle,
    /// `(name, ip)` pairs for the workloads.
    pub(crate) workload_info: &'a [(String, String)],
    /// Per-workload client identity info parallel to `workload_info`.
    pub(crate) client_info: &'a [WorkloadClientInfo],
    /// Optional process configuration (booted server processes).
    pub(crate) process_config: Option<ProcessConfig<'a>>,
    /// Iteration seed.
    pub(crate) seed: u64,
    /// Simulation world (consumed and driven through phases).
    pub(crate) sim: crate::sim::SimWorld,
    /// Optional chaos duration; `None` disables fault injection.
    pub(crate) chaos_duration: Option<Duration>,
    /// Iteration count (used for diagnostics on deadlock).
    pub(crate) iteration_count: usize,
}

/// Successful output of [`WorkloadOrchestrator::orchestrate_workloads`].
pub(crate) struct OrchestrateOutput {
    /// Workloads returned to the caller for reuse.
    pub(crate) workloads: Vec<Box<dyn Workload>>,
    /// Fault injectors returned to the caller for reuse.
    pub(crate) fault_injectors: Vec<Box<dyn FaultInjector>>,
    /// Per-workload results from setup + run + check.
    pub(crate) results: Vec<SimulationResult<()>>,
    /// Simulation metrics extracted from `sim`.
    pub(crate) metrics: SimulationMetrics,
}

/// Inputs to [`WorkloadOrchestrator::finalize_orchestration`].
struct FinalizeOrchestration<'a, 'pm> {
    sim: &'a mut crate::sim::SimWorld,
    process_manager: &'a mut ProcessManager<'pm>,
    returned_workloads: Vec<Box<dyn Workload>>,
    returned_injectors: Vec<Box<dyn FaultInjector>>,
    results: Vec<SimulationResult<()>>,
    seed: u64,
    state: &'a StateHandle,
    obs: &'a SimulationLayerHandle,
    shutdown_signal: &'a tokio_util::sync::CancellationToken,
    workload_info: &'a [(String, String)],
    client_info: &'a [WorkloadClientInfo],
    all_entities: &'a [(String, String)],
    process_ips: &'a [String],
    tag_registry: &'a TagRegistry,
}

/// Topology metadata derived from a workload/process configuration.
struct TopologyMetadata {
    process_ips: Vec<String>,
    tag_registry: TagRegistry,
    all_entities: Vec<(String, String)>,
}

/// Inputs to [`WorkloadOrchestrator::boot_and_setup`].
struct BootAndSetupInputs<'a, 'pm> {
    process_config: Option<ProcessConfig<'pm>>,
    workloads: Vec<Box<dyn Workload>>,
    workload_info: &'a [(String, String)],
    client_info: &'a [WorkloadClientInfo],
    all_entities: &'a [(String, String)],
    process_ips: &'a [String],
    tag_registry: &'a TagRegistry,
    sim: &'a mut crate::sim::SimWorld,
    seed: u64,
    state: &'a StateHandle,
    obs: &'a SimulationLayerHandle,
    shutdown_signal: &'a tokio_util::sync::CancellationToken,
}

/// Result of [`WorkloadOrchestrator::boot_and_setup`]: continue running or
/// early-exit because setup failed.
enum BootAndSetupOutcome<'pm> {
    /// Setup succeeded; continue into the chaos + run phase.
    Continue {
        workloads: Vec<Box<dyn Workload>>,
        contexts: Vec<SimContext>,
        process_manager: ProcessManager<'pm>,
    },
    /// Setup failed; surface the partial results to the caller.
    SetupFailed {
        workloads: Vec<Box<dyn Workload>>,
        results: Vec<SimulationResult<()>>,
    },
}

/// Inputs to [`WorkloadOrchestrator::do_chaos_and_run_phase`].
struct ChaosAndRunInputs<'a, 'pm> {
    sim: &'a mut crate::sim::SimWorld,
    process_manager: &'a mut ProcessManager<'pm>,
    workloads: Vec<Box<dyn Workload>>,
    contexts: Vec<SimContext>,
    fault_injectors: Vec<Box<dyn FaultInjector>>,
    chaos_duration: Option<Duration>,
    all_entities: &'a [(String, String)],
    state: &'a StateHandle,
    obs: &'a SimulationLayerHandle,
    shutdown_signal: &'a tokio_util::sync::CancellationToken,
    seed: u64,
    iteration_count: usize,
}

/// Output of [`WorkloadOrchestrator::do_chaos_and_run_phase`].
struct ChaosAndRunOutput {
    returned_workloads: Vec<Box<dyn Workload>>,
    returned_injectors: Vec<Box<dyn FaultInjector>>,
    results: Vec<SimulationResult<()>>,
}

impl WorkloadOrchestrator {
    /// Execute all workloads using the unified lifecycle:
    /// boot → setup → run (with optional chaos) → settle → check.
    ///
    /// Setup and check run inside the cooperative event loop so network
    /// RPCs don't deadlock. When `chaos_duration` is set, fault injectors
    /// run concurrently with workloads and stop when the duration elapses.
    /// After all workloads complete, a settle phase drains remaining events.
    ///
    /// Returns workloads and fault injectors back to the caller for reuse across iterations.
    pub(crate) async fn orchestrate_workloads(
        inputs: OrchestrateInputs<'_>,
    ) -> Result<OrchestrateOutput, (Vec<u64>, usize)> {
        let OrchestrateInputs {
            workloads,
            fault_injectors,
            obs,
            workload_info,
            client_info,
            process_config,
            seed,
            mut sim,
            chaos_duration,
            iteration_count,
        } = inputs;

        tracing::debug!(
            "Orchestrating {} workload(s), {} fault injector(s), {} process(es)",
            workloads.len(),
            fault_injectors.len(),
            process_config.as_ref().map_or(0, |pc| pc.ips.len()),
        );

        let TopologyMetadata {
            process_ips,
            tag_registry,
            all_entities,
        } = Self::build_topology_metadata(workload_info, process_config.as_ref());

        // Shared state for cross-workload publish/get communication. Event
        // timelines and invariants live on `obs` (SimulationLayer).
        let state = StateHandle::new();
        let shutdown_signal = tokio_util::sync::CancellationToken::new();

        let (workloads, contexts, mut process_manager) =
            match Self::boot_and_setup(BootAndSetupInputs {
                process_config,
                workloads,
                workload_info,
                client_info,
                all_entities: &all_entities,
                process_ips: &process_ips,
                tag_registry: &tag_registry,
                sim: &mut sim,
                seed,
                state: &state,
                obs: &obs,
                shutdown_signal: &shutdown_signal,
            })
            .await?
            {
                BootAndSetupOutcome::Continue {
                    workloads,
                    contexts,
                    process_manager,
                } => (workloads, contexts, process_manager),
                BootAndSetupOutcome::SetupFailed { workloads, results } => {
                    return Ok(OrchestrateOutput {
                        workloads,
                        fault_injectors,
                        results,
                        metrics: sim.extract_metrics(),
                    });
                }
            };

        let ChaosAndRunOutput {
            returned_workloads,
            returned_injectors,
            results,
        } = Self::do_chaos_and_run_phase(ChaosAndRunInputs {
            sim: &mut sim,
            process_manager: &mut process_manager,
            workloads,
            contexts,
            fault_injectors,
            chaos_duration,
            all_entities: &all_entities,
            state: &state,
            obs: &obs,
            shutdown_signal: &shutdown_signal,
            seed,
            iteration_count,
        })
        .await?;

        Self::finalize_orchestration(FinalizeOrchestration {
            sim: &mut sim,
            process_manager: &mut process_manager,
            returned_workloads,
            returned_injectors,
            results,
            seed,
            state: &state,
            obs: &obs,
            shutdown_signal: &shutdown_signal,
            workload_info,
            client_info,
            all_entities: &all_entities,
            process_ips: &process_ips,
            tag_registry: &tag_registry,
        })
        .await
    }

    /// Build topology metadata (process IPs, tag registry, combined entity
    /// list) from the workload info and optional process config.
    fn build_topology_metadata(
        workload_info: &[(String, String)],
        process_config: Option<&ProcessConfig<'_>>,
    ) -> TopologyMetadata {
        let process_ips = process_config.map(|pc| pc.ips.clone()).unwrap_or_default();
        let tag_registry = process_config
            .map(|pc| pc.tag_registry.clone())
            .unwrap_or_default();
        let all_entities = workload_info
            .iter()
            .chain(process_config.map_or(&[][..], |pc| pc.info.as_slice()))
            .cloned()
            .collect();
        TopologyMetadata {
            process_ips,
            tag_registry,
            all_entities,
        }
    }

    /// Boot processes, build per-workload contexts, and run the setup phase.
    /// Returns either the state needed to continue into the run phase, or an
    /// early-exit signal if setup failed.
    async fn boot_and_setup<'pm>(
        inputs: BootAndSetupInputs<'_, 'pm>,
    ) -> Result<BootAndSetupOutcome<'pm>, (Vec<u64>, usize)> {
        let BootAndSetupInputs {
            process_config,
            workloads,
            workload_info,
            client_info,
            all_entities,
            process_ips,
            tag_registry,
            sim,
            seed,
            state,
            obs,
            shutdown_signal,
        } = inputs;

        let mut process_manager = Self::boot_and_wrap_process_manager(
            process_config,
            all_entities,
            sim,
            seed,
            state,
            obs,
            shutdown_signal,
        )
        .map_err(|()| (vec![seed], 1usize))?;

        let contexts = Self::build_workload_contexts(&WorkloadContextEnv {
            workload_info,
            client_info,
            all_entities,
            process_ips,
            tag_registry,
            shutdown_signal,
            sim,
            seed,
            state,
            obs,
        })
        .map_err(|()| (vec![seed], 1usize))?;

        let (workloads, contexts, setup_results, setup_failed) = Self::do_setup_phase(
            workloads,
            contexts,
            PhaseEnv {
                sim,
                process_manager: &mut process_manager,
                seed,
                state,
                obs,
                shutdown_signal,
            },
        )
        .await;
        if setup_failed {
            process_manager.abort_all();
            return Ok(BootAndSetupOutcome::SetupFailed {
                workloads,
                results: setup_results,
            });
        }
        Ok(BootAndSetupOutcome::Continue {
            workloads,
            contexts,
            process_manager,
        })
    }

    /// Run sections 3 (start fault injectors) + 4 (cooperative run loop)
    /// and collect the per-workload + per-injector results.
    async fn do_chaos_and_run_phase(
        inputs: ChaosAndRunInputs<'_, '_>,
    ) -> Result<ChaosAndRunOutput, (Vec<u64>, usize)> {
        let ChaosAndRunInputs {
            sim,
            process_manager,
            workloads,
            contexts,
            fault_injectors,
            chaos_duration,
            all_entities,
            state,
            obs,
            shutdown_signal,
            seed,
            iteration_count,
        } = inputs;

        let chaos_shutdown = tokio_util::sync::CancellationToken::new();
        let all_ips: Vec<String> = all_entities.iter().map(|(_, ip)| ip.clone()).collect();
        let (mut injector_handles, parked_injectors) = Self::start_fault_injectors(
            fault_injectors,
            chaos_duration,
            sim,
            process_manager,
            seed,
            &chaos_shutdown,
        )
        .map_err(|()| (vec![seed], 1usize))?;

        let total_workloads = workloads.len();
        let mut workload_handles: WorkloadHandleSlots = Self::spawn_run_tasks(workloads, contexts);
        let mut workload_collected: Vec<Option<WorkloadResult>> =
            (0..total_workloads).map(|_| None).collect();
        Self::drive_run_phase(RunPhaseInputs {
            sim,
            process_manager,
            obs,
            state,
            shutdown_signal,
            chaos_shutdown: &chaos_shutdown,
            chaos_duration,
            all_ips: &all_ips,
            workload_handles: &mut workload_handles,
            workload_collected: &mut workload_collected,
            injector_handles: &mut injector_handles,
            seed,
            iteration_count,
        })
        .await?;

        let returned_injectors =
            Self::collect_injector_results(parked_injectors, injector_handles).await;
        let (returned_workloads, results) =
            Self::collect_workload_results(workload_collected, total_workloads);
        Ok(ChaosAndRunOutput {
            returned_workloads,
            returned_injectors,
            results,
        })
    }

    /// Run sections 5 (abort) + 6 (settle) + 7 (check) and the optional
    /// explorer exit. Returns the final orchestration output.
    async fn finalize_orchestration(
        inputs: FinalizeOrchestration<'_, '_>,
    ) -> Result<OrchestrateOutput, (Vec<u64>, usize)> {
        let FinalizeOrchestration {
            sim,
            process_manager,
            returned_workloads,
            returned_injectors,
            results,
            seed,
            state,
            obs,
            shutdown_signal,
            workload_info,
            client_info,
            all_entities,
            process_ips,
            tag_registry,
        } = inputs;

        // === 5. ABORT ALL PROCESSES ===
        process_manager.abort_all();

        // === 6. SETTLE ===
        if let Some(settle_err) = Self::settle_phase(sim) {
            return Ok(OrchestrateOutput {
                workloads: returned_workloads,
                fault_injectors: returned_injectors,
                results: vec![Err(settle_err)],
                metrics: sim.extract_metrics(),
            });
        }

        // === 7. CHECK PHASE (tokio::spawn + cooperative stepping) ===
        let final_workloads = Self::do_check_phase(CheckPhaseInputs {
            sim,
            workloads: returned_workloads,
            workload_info,
            client_info,
            all_entities,
            process_ips,
            tag_registry,
            shutdown_signal,
            seed,
            state,
            obs,
        })
        .await
        .map_err(|()| (vec![seed], 1usize))?;
        let metrics = sim.extract_metrics();

        Self::maybe_exit_child(&results);

        Ok(OrchestrateOutput {
            workloads: final_workloads,
            fault_injectors: returned_injectors,
            results,
            metrics,
        })
    }

    /// If running as a forked explorer child, exit with status 0 on success
    /// (all `Ok` results + no `assert_always!` violations) and 42 otherwise.
    /// Returns to the caller when not a forked child.
    fn maybe_exit_child(results: &[SimulationResult<()>]) {
        if !moonpool_explorer::explorer_is_child() {
            return;
        }
        let success = results.iter().all(std::result::Result::is_ok)
            && !crate::chaos::has_always_violations();
        let code = if success { 0 } else { 42 };
        moonpool_explorer::exit_child(code);
    }

    /// Run the entire check phase: build per-workload contexts, spawn
    /// `check()` futures, drive the cooperative loop, and collect the
    /// resulting workloads.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if a workload IP fails to parse.
    async fn do_check_phase(inputs: CheckPhaseInputs<'_>) -> Result<Vec<Box<dyn Workload>>, ()> {
        let CheckPhaseInputs {
            sim,
            workloads,
            workload_info,
            client_info,
            all_entities,
            process_ips,
            tag_registry,
            shutdown_signal,
            seed,
            state,
            obs,
        } = inputs;
        let check_contexts = Self::build_workload_contexts(&WorkloadContextEnv {
            workload_info,
            client_info,
            all_entities,
            process_ips,
            tag_registry,
            shutdown_signal,
            sim,
            seed,
            state,
            obs,
        })?;
        Ok(Self::run_check_phase(sim, workloads, check_contexts).await)
    }

    /// Run the entire setup phase: spawn `setup()` futures, drive the
    /// cooperative loop, and collect results.
    async fn do_setup_phase(
        workloads: Vec<Box<dyn Workload>>,
        contexts: Vec<SimContext>,
        env: PhaseEnv<'_, '_>,
    ) -> (
        Vec<Box<dyn Workload>>,
        Vec<SimContext>,
        Vec<SimulationResult<()>>,
        bool,
    ) {
        let setup_handles = Self::spawn_setup_tasks(workloads, contexts);
        Self::cooperative_loop_until_done(
            env.sim,
            env.process_manager,
            env.seed,
            env.state,
            env.obs,
            env.shutdown_signal,
            &setup_handles,
        )
        .await;
        Self::collect_setup_results(setup_handles).await
    }

    /// Spawn each workload's `setup()` future as a tokio task and collect
    /// the join handles.
    fn spawn_setup_tasks(
        workloads: Vec<Box<dyn Workload>>,
        contexts: Vec<SimContext>,
    ) -> Vec<SetupHandle> {
        let mut setup_handles = Vec::with_capacity(workloads.len());
        for (workload, ctx) in workloads.into_iter().zip(contexts) {
            let handle = tokio::spawn(async move {
                let mut w = workload;
                let result = w.setup(&ctx).await;
                (w, ctx, result)
            });
            setup_handles.push(handle);
        }
        setup_handles
    }

    /// Spawn each workload's `run()` future as a tokio task and collect
    /// the join handles in option slots.
    fn spawn_run_tasks(
        workloads: Vec<Box<dyn Workload>>,
        contexts: Vec<SimContext>,
    ) -> WorkloadHandleSlots {
        let mut workload_handles = Vec::with_capacity(workloads.len());
        for (workload, ctx) in workloads.into_iter().zip(contexts) {
            let handle = tokio::spawn(async move {
                let mut w = workload;
                let result = w.run(&ctx).await;
                (w, result)
            });
            workload_handles.push(Some(handle));
        }
        workload_handles
    }

    /// Drive the unified cooperative run-phase loop until every workload
    /// task has completed (or a deadlock forces the loop to bail out).
    async fn drive_run_phase(inputs: RunPhaseInputs<'_, '_>) -> Result<(), (Vec<u64>, usize)> {
        let RunPhaseInputs {
            sim,
            process_manager,
            obs,
            state,
            shutdown_signal,
            chaos_shutdown,
            chaos_duration,
            all_ips,
            workload_handles,
            workload_collected,
            injector_handles,
            seed,
            iteration_count,
        } = inputs;

        let chaos_start = sim.current_time();
        let mut chaos_ended = chaos_duration.is_none();
        let mut deadlock_detector = DeadlockDetector::new(3);
        let mut shutdown_triggered = false;
        let mut loop_count: u64 = 0;

        loop {
            let active_workloads = workload_handles.iter().filter(|h| h.is_some()).count();
            if active_workloads == 0 {
                break;
            }

            loop_count += 1;
            if loop_count.is_multiple_of(100) {
                tracing::debug!(
                    "Cooperative loop iteration {}, {} handles active, {} pending events",
                    loop_count,
                    active_workloads,
                    sim.pending_event_count()
                );
            }

            let initial_handle_count = active_workloads;
            let initial_event_count = sim.pending_event_count();

            if !chaos_ended && Self::should_end_chaos(sim, chaos_start, chaos_duration) {
                tracing::debug!("Chaos phase ended");
                chaos_shutdown.cancel();
                Self::heal_all_partitions(sim, all_ips);
                chaos_ended = true;
                assert_reachable!("phase: chaos ended");
            }

            if sim.pending_event_count() > 0 {
                sim.step();
                obs.set_sim_time_ms(
                    u64::try_from(sim.current_time().as_millis()).unwrap_or(u64::MAX),
                );
                Self::handle_process_events(
                    sim,
                    process_manager,
                    seed,
                    state,
                    obs,
                    shutdown_signal,
                );
            }

            let any_finished =
                Self::collect_finished_workloads(workload_handles, workload_collected).await;

            if any_finished && !shutdown_triggered {
                Self::trigger_shutdown(sim, shutdown_signal);
                shutdown_triggered = true;
            }

            Self::reap_finished_injectors(injector_handles).await;

            let current_active = workload_handles.iter().filter(|h| h.is_some()).count();

            if deadlock_detector.check_deadlock(
                current_active,
                initial_handle_count,
                sim.pending_event_count(),
                initial_event_count,
            ) {
                if shutdown_triggered {
                    tracing::error!(
                        "DEADLOCK detected on iteration {} with seed {}: {} tasks remaining after {} no-progress iterations",
                        iteration_count,
                        seed,
                        current_active,
                        deadlock_detector.no_progress_count()
                    );
                    return Err((vec![seed], 1));
                }
                tracing::warn!(
                    "No progress detected on iteration {} with seed {}: {} tasks remaining. Triggering shutdown to unblock workloads.",
                    iteration_count,
                    seed,
                    current_active,
                );
                Self::trigger_shutdown(sim, shutdown_signal);
                shutdown_triggered = true;
                deadlock_detector.reset();
            }

            if current_active > 0 {
                tokio::task::yield_now().await;
            }
        }
        Ok(())
    }

    /// Spawn the fault injectors for the chaos phase. When `chaos_duration`
    /// is `None`, the injectors are returned in `parked_injectors` instead.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if the simulation has already been shut down.
    fn start_fault_injectors(
        fault_injectors: Vec<Box<dyn FaultInjector>>,
        chaos_duration: Option<Duration>,
        sim: &crate::sim::SimWorld,
        process_manager: &ProcessManager<'_>,
        seed: u64,
        chaos_shutdown: &tokio_util::sync::CancellationToken,
    ) -> Result<(InjectorHandleSlots, Vec<Box<dyn FaultInjector>>), ()> {
        let mut injector_handles: InjectorHandleSlots = Vec::new();
        let mut parked_injectors: Vec<Box<dyn FaultInjector>> = Vec::new();
        if chaos_duration.is_some() {
            for fi in fault_injectors {
                let fault_sim = sim.downgrade().upgrade().map_err(|_| ())?;
                let fault_ctx = FaultContext::new(
                    fault_sim,
                    crate::runner::fault_injector::ProcessInfo {
                        process_ips: process_manager.ips.clone(),
                        tag_registry: process_manager.tag_registry.clone(),
                        dead_count: process_manager.dead_count(),
                    },
                    crate::SimRandomProvider::new(seed),
                    sim.time_provider(),
                    chaos_shutdown.clone(),
                );
                let handle = tokio::spawn(async move {
                    let mut injector = fi;
                    let result = injector.inject(&fault_ctx).await;
                    (injector, result)
                });
                injector_handles.push(Some(handle));
            }
        } else {
            parked_injectors = fault_injectors;
        }
        Ok((injector_handles, parked_injectors))
    }

    /// Boot processes and wrap them in a [`ProcessManager`] for lifecycle
    /// management. Returns an empty manager when `process_config` is `None`.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if a process IP fails to parse during boot.
    fn boot_and_wrap_process_manager<'pm>(
        process_config: Option<ProcessConfig<'pm>>,
        all_entities: &[(String, String)],
        sim: &crate::sim::SimWorld,
        seed: u64,
        state: &StateHandle,
        obs: &SimulationLayerHandle,
        shutdown_signal: &tokio_util::sync::CancellationToken,
    ) -> Result<ProcessManager<'pm>, ()> {
        let (process_handles, process_tokens) = Self::boot_processes(
            process_config.as_ref(),
            all_entities,
            sim,
            seed,
            state,
            obs,
            shutdown_signal,
        )?;
        Ok(match process_config {
            Some(pc) => ProcessManager::new(
                pc.factory,
                process_handles,
                process_tokens,
                pc.ips,
                pc.tag_registry,
                all_entities.to_vec(),
            ),
            None => ProcessManager::new_empty(),
        })
    }

    /// Boot all configured processes, spawning a task per process and
    /// returning the per-process join handles and per-process cancellation
    /// tokens.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if a process IP fails to parse.
    fn boot_processes(
        process_config: Option<&ProcessConfig<'_>>,
        all_entities: &[(String, String)],
        sim: &crate::sim::SimWorld,
        seed: u64,
        state: &StateHandle,
        obs: &SimulationLayerHandle,
        shutdown_signal: &tokio_util::sync::CancellationToken,
    ) -> Result<(ProcessHandleSlots, ProcessTokenSlots), ()> {
        let mut process_handles: ProcessHandleSlots = Vec::new();
        let mut process_tokens: ProcessTokenSlots = Vec::new();
        let Some(pc) = process_config else {
            return Ok((process_handles, process_tokens));
        };
        for (i, ip) in pc.ips.iter().enumerate() {
            let mut process = (pc.factory)();
            let ip_addr: std::net::IpAddr = ip.parse().map_err(|_| ())?;
            let process_tags = pc
                .tag_registry
                .tags_for(ip_addr)
                .cloned()
                .unwrap_or_default();
            // Per-process token: child of global shutdown_signal.
            let process_token = shutdown_signal.child_token();
            let topology = TopologyFactory::create_topology_with_processes(TopologyInputs {
                ip,
                client_id: i,
                client_count: pc.ips.len(),
                all_entities,
                process_ips: &pc.ips,
                my_tags: process_tags,
                tag_registry: pc.tag_registry.clone(),
                shutdown_signal: process_token.clone(),
            });
            let providers = crate::SimProviders::new(sim.downgrade(), seed, ip_addr);
            let ctx = SimContext::new(providers, topology, state.clone(), obs.clone());
            let ip_for_log = ip.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = process.run(&ctx).await {
                    tracing::debug!("Process at {} exited: {}", ip_for_log, e);
                }
            });
            process_handles.push(Some(handle));
            process_tokens.push(Some(process_token));
            tracing::debug!("Booted process {} at {}", i, ip);
        }
        Ok((process_handles, process_tokens))
    }

    /// Spawn the check phase tasks, drive them cooperatively, and collect
    /// the resulting workloads.
    async fn run_check_phase(
        sim: &mut crate::sim::SimWorld,
        workloads: Vec<Box<dyn Workload>>,
        contexts: Vec<SimContext>,
    ) -> Vec<Box<dyn Workload>> {
        let mut check_handles = Vec::with_capacity(workloads.len());
        for (workload, ctx) in workloads.into_iter().zip(contexts) {
            let handle = tokio::spawn(async move {
                let mut w = workload;
                let result = w.check(&ctx).await;
                if let Err(ref e) = result {
                    tracing::error!("Workload '{}' check failed: {}", w.name(), e);
                }
                w
            });
            check_handles.push(handle);
        }

        // Cooperative loop for check.
        loop {
            if check_handles
                .iter()
                .all(tokio::task::JoinHandle::is_finished)
            {
                break;
            }
            if sim.pending_event_count() > 0 {
                sim.step();
            }
            tokio::task::yield_now().await;
        }

        // Collect check results.
        let mut final_workloads = Vec::with_capacity(check_handles.len());
        for handle in check_handles {
            match handle.await {
                Ok(w) => final_workloads.push(w),
                Err(_) => {
                    tracing::error!("Check task panicked");
                }
            }
        }
        final_workloads
    }

    /// Build per-workload [`SimContext`]s for the run/check phases.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if a workload IP fails to parse.
    fn build_workload_contexts(env: &WorkloadContextEnv<'_>) -> Result<Vec<SimContext>, ()> {
        let mut contexts = Vec::with_capacity(env.workload_info.len());
        for (i, (_, ip)) in env.workload_info.iter().enumerate() {
            let WorkloadClientInfo {
                client_id,
                client_count,
            } = env.client_info[i];
            let ip_addr: std::net::IpAddr = ip.parse().map_err(|_| ())?;
            let topology = TopologyFactory::create_topology_with_processes(TopologyInputs {
                ip,
                client_id,
                client_count,
                all_entities: env.all_entities,
                process_ips: env.process_ips,
                my_tags: ProcessTags::default(),
                tag_registry: env.tag_registry.clone(),
                shutdown_signal: env.shutdown_signal.clone(),
            });
            let providers = crate::SimProviders::new(env.sim.downgrade(), env.seed, ip_addr);
            let ctx = SimContext::new(providers, topology, env.state.clone(), env.obs.clone());
            contexts.push(ctx);
        }
        Ok(contexts)
    }

    /// Returns `true` once enough simulation time has elapsed to end the
    /// chaos phase, given the chaos start time and configured duration.
    fn should_end_chaos(
        sim: &crate::sim::SimWorld,
        chaos_start: Duration,
        chaos_duration: Option<Duration>,
    ) -> bool {
        let elapsed = sim.current_time().saturating_sub(chaos_start);
        chaos_duration.is_some_and(|cd| elapsed >= cd)
    }

    /// Move every finished workload handle out of `workload_handles` into
    /// `workload_collected`. Returns `true` if at least one handle finished
    /// during this call.
    async fn collect_finished_workloads(
        workload_handles: &mut [Option<tokio::task::JoinHandle<WorkloadResult>>],
        workload_collected: &mut [Option<WorkloadResult>],
    ) -> bool {
        let mut any_finished = false;
        for i in 0..workload_handles.len() {
            let finished = workload_handles[i]
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished);
            if finished {
                let handle = workload_handles[i]
                    .take()
                    .expect("workload handle is finished");
                match handle.await {
                    Ok((workload, result)) => {
                        tracing::debug!("Workload '{}' completed", workload.name());
                        workload_collected[i] = Some((workload, result));
                    }
                    Err(_) => {
                        tracing::error!("Workload task panicked");
                    }
                }
                any_finished = true;
            }
        }
        any_finished
    }

    /// Reap any fault-injector handles that have finished, discarding the
    /// injectors. The remaining live handles are returned in-place.
    async fn reap_finished_injectors(
        injector_handles: &mut [Option<tokio::task::JoinHandle<InjectorResult>>],
    ) {
        for handle_opt in injector_handles {
            let finished = handle_opt
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished);
            if finished {
                let handle = handle_opt.take().expect("injector handle is finished");
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
    }

    /// Collect remaining fault injector results, aborting any handles that
    /// are still running.
    async fn collect_injector_results(
        mut returned: Vec<Box<dyn FaultInjector>>,
        mut injector_handles: Vec<Option<tokio::task::JoinHandle<InjectorResult>>>,
    ) -> Vec<Box<dyn FaultInjector>> {
        for handle_opt in &mut injector_handles {
            if let Some(handle) = handle_opt.take() {
                if handle.is_finished() {
                    if let Ok((injector, _)) = handle.await {
                        returned.push(injector);
                    }
                } else {
                    handle.abort();
                }
            }
        }
        returned
    }

    /// Build the final workload return list, substituting `Err` for any
    /// slots that panicked.
    fn collect_workload_results(
        workload_collected: Vec<Option<WorkloadResult>>,
        total_workloads: usize,
    ) -> (Vec<Box<dyn Workload>>, Vec<SimulationResult<()>>) {
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
        (returned_workloads, results)
    }

    /// Drive the simulation cooperatively until every handle in `handles`
    /// reports finished.
    async fn cooperative_loop_until_done<T: 'static>(
        sim: &mut crate::sim::SimWorld,
        process_manager: &mut ProcessManager<'_>,
        seed: u64,
        state: &StateHandle,
        obs: &SimulationLayerHandle,
        shutdown_signal: &tokio_util::sync::CancellationToken,
        handles: &[tokio::task::JoinHandle<T>],
    ) {
        loop {
            if handles.iter().all(tokio::task::JoinHandle::is_finished) {
                break;
            }
            if sim.pending_event_count() > 0 {
                sim.step();
                Self::handle_process_events(
                    sim,
                    process_manager,
                    seed,
                    state,
                    obs,
                    shutdown_signal,
                );
            }
            tokio::task::yield_now().await;
        }
    }

    /// Collect results from spawned `setup()` tasks.
    async fn collect_setup_results(
        setup_handles: Vec<SetupHandle>,
    ) -> (
        Vec<Box<dyn Workload>>,
        Vec<SimContext>,
        Vec<SimulationResult<()>>,
        bool,
    ) {
        let mut workloads = Vec::with_capacity(setup_handles.len());
        let mut contexts = Vec::with_capacity(setup_handles.len());
        let mut setup_failed = false;
        let mut setup_results: Vec<SimulationResult<()>> = Vec::new();
        for handle in setup_handles {
            if let Ok((w, ctx, result)) = handle.await {
                if let Err(ref e) = result {
                    tracing::error!("Workload '{}' setup failed: {}", w.name(), e);
                    setup_failed = true;
                }
                setup_results.push(result);
                workloads.push(w);
                contexts.push(ctx);
            } else {
                tracing::error!("Setup task panicked");
                setup_failed = true;
                setup_results.push(Err(crate::SimulationError::InvalidState(
                    "Setup task panicked".to_string(),
                )));
            }
        }
        (workloads, contexts, setup_results, setup_failed)
    }

    /// Drain remaining simulation events synchronously after all workloads
    /// have completed.
    ///
    /// Returns `Some(SettleTimeout)` if the queue does not converge within
    /// the timeout, otherwise `None` on a clean drain.
    fn settle_phase(sim: &mut crate::sim::SimWorld) -> Option<crate::SimulationError> {
        // Synchronous drain: process all remaining events without yielding.
        // No yield means no tasks can schedule new events, so the queue
        // converges to empty.
        let settle_start = sim.current_time();
        let settle_timeout = Duration::from_mins(5);

        while sim.pending_event_count() > 0 {
            let elapsed = sim.current_time().saturating_sub(settle_start);
            if elapsed > settle_timeout {
                tracing::error!(
                    "Settle timeout: {} events still pending after {:?}",
                    sim.pending_event_count(),
                    elapsed
                );
                return Some(crate::SimulationError::SettleTimeout {
                    pending_events: sim.pending_event_count(),
                    elapsed,
                });
            }
            sim.step();
        }
        None
    }

    /// Handle process lifecycle events from the last simulation step.
    fn handle_process_events(
        sim: &mut crate::sim::SimWorld,
        process_manager: &mut ProcessManager<'_>,
        seed: u64,
        state: &StateHandle,
        obs: &SimulationLayerHandle,
        shutdown_signal: &tokio_util::sync::CancellationToken,
    ) {
        match sim.last_processed_event() {
            Some(crate::sim::Event::ProcessGracefulShutdown {
                ip,
                grace_period_ms,
                recovery_delay_ms,
            }) => {
                assert_reachable!("event: ProcessGracefulShutdown");
                let event = SimFaultEvent::ProcessGracefulShutdown {
                    ip: ip.to_string(),
                    grace_period_ms,
                };
                tracing::info!(
                    capture = true,
                    trail = SIM_FAULT_TRAIL,
                    source = "sim",
                    event = valuable(&event),
                );
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
                let event = SimFaultEvent::ProcessForceKill { ip: ip.to_string() };
                tracing::info!(
                    capture = true,
                    trail = SIM_FAULT_TRAIL,
                    source = "sim",
                    event = valuable(&event),
                );
                process_manager.abort_process(ip);
                sim.abort_all_connections_for_ip(ip);
                sim.schedule_process_restart(ip, Duration::from_millis(recovery_delay_ms));
            }
            Some(crate::sim::Event::ProcessRestart { ip }) => {
                assert_reachable!("event: ProcessRestart");
                let event = SimFaultEvent::ProcessRestart { ip: ip.to_string() };
                tracing::info!(
                    capture = true,
                    trail = SIM_FAULT_TRAIL,
                    source = "sim",
                    event = valuable(&event),
                );
                let weak_sim = sim.downgrade();
                process_manager.handle_restart(ip, &weak_sim, seed, state, obs, shutdown_signal);
            }
            _ => {}
        }
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
            .map_or(12345, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));

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
            super::builder::IterationControl::FixedCount(count)
            | super::builder::IterationControl::UntilConverged {
                max_iterations: count,
            }
            | super::builder::IterationControl::CoveragePlateau {
                max_iterations: count,
                ..
            } => self.iteration_count < *count,
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
                super::builder::IterationControl::UntilConverged { max_iterations } => {
                    *max_iterations
                }
                super::builder::IterationControl::CoveragePlateau { max_iterations, .. } =>
                    *max_iterations,
            }
        );

        seed
    }

    /// Get the current iteration count.
    pub(crate) fn current_iteration(&self) -> usize {
        self.iteration_count
    }

    /// Get the known maximum number of iterations, or `None` for time-based control.
    pub(crate) fn max_iterations(&self) -> Option<usize> {
        match &self.control {
            super::builder::IterationControl::FixedCount(count)
            | super::builder::IterationControl::UntilConverged {
                max_iterations: count,
            }
            | super::builder::IterationControl::CoveragePlateau {
                max_iterations: count,
                ..
            } => Some(*count),
            super::builder::IterationControl::TimeLimit(_) => None,
        }
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
        let workloads_ok = all_results.iter().all(std::result::Result::is_ok);
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
                "One or more workloads failed (seed {seed})"
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
        inputs: GenerateReportInputs,
    ) -> super::report::SimulationReport {
        let GenerateReportInputs {
            iteration_count,
            seeds_used,
            assertion_results,
            assertion_violations,
            coverage_violations,
            exploration,
            assertion_details,
            bucket_summaries,
            convergence_timeout,
        } = inputs;
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
            convergence_timeout,
        }
    }
}

/// Inputs needed to build a [`super::report::SimulationReport`] from a
/// [`MetricsCollector`].
pub(crate) struct GenerateReportInputs {
    /// Total iterations executed.
    pub(crate) iteration_count: usize,
    /// Seeds that ran during the simulation.
    pub(crate) seeds_used: Vec<u64>,
    /// Aggregated per-name assertion statistics.
    pub(crate) assertion_results: HashMap<String, AssertionStats>,
    /// Names of assertion contracts that failed.
    pub(crate) assertion_violations: Vec<String>,
    /// Names of coverage assertions that were never reached.
    pub(crate) coverage_violations: Vec<String>,
    /// Optional exploration (multiverse) report.
    pub(crate) exploration: Option<super::report::ExplorationReport>,
    /// Per-assertion details collected from shared memory.
    pub(crate) assertion_details: Vec<super::report::AssertionDetail>,
    /// Per-`each_bucket` site summaries collected from shared memory.
    pub(crate) bucket_summaries: Vec<super::report::BucketSiteSummary>,
    /// Whether the simulation stopped due to a convergence timeout.
    pub(crate) convergence_timeout: bool,
}
