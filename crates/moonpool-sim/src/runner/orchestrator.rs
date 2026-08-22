//! Workload orchestration and iteration management.
//!
//! This module provides utilities for orchestrating workload execution
//! and managing simulation iterations.

use std::time::Duration;

use tracing::Instrument as _;

use crate::chaos::fault_events::SimFaultEvent;
use crate::chaos::state_handle::StateHandle;
use crate::observability::SimulationLayerHandle;
use crate::runner::builder::WorkloadClientInfo;
use crate::runner::context::SimContext;
use crate::runner::fault_injector::{FaultContext, FaultInjector};
use crate::runner::locality::MachineRegistry;
use crate::runner::tags::{ProcessTags, TagRegistry};
use crate::runner::topology::{TopologyFactory, TopologyInputs};
use crate::runner::workload::Workload;
use crate::{SimulationResult, assert_reachable};

use super::process_manager::{ProcessConfig, ProcessManager};
use super::report::SimulationMetrics;
use super::stall::{RunStallGuard, StallOutcome};

/// Orchestrates workload execution and event processing.
pub(crate) struct WorkloadOrchestrator;

/// Result of a completed workload task.
type WorkloadResult = (Box<dyn Workload>, SimulationResult<()>);

/// Result returned by a spawned `setup()` task: the workload, its context,
/// and the setup result.
type SetupTaskOutput = (Box<dyn Workload>, SimContext, SimulationResult<()>);

/// Handle to a spawned `setup()` task.
type SetupHandle = crate::executor::JoinHandle<SetupTaskOutput>;

/// Per-process join handles (in option slots so they can be drained).
type ProcessHandleSlots = Vec<Option<crate::executor::JoinHandle<()>>>;

/// Per-process cancellation tokens (in option slots so they can be drained).
type ProcessTokenSlots = Vec<Option<tokio_util::sync::CancellationToken>>;

/// Per-injector join handles (in option slots so they can be drained).
type InjectorHandleSlots = Vec<Option<crate::executor::JoinHandle<InjectorResult>>>;

/// Per-workload join handles (in option slots so they can be drained).
type WorkloadHandleSlots = Vec<Option<crate::executor::JoinHandle<WorkloadResult>>>;

/// Inputs needed to run the check phase.
struct CheckPhaseInputs<'a> {
    sim: &'a mut crate::sim::SimWorld,
    workloads: Vec<Box<dyn Workload>>,
    workload_info: &'a [(String, String)],
    client_info: &'a [WorkloadClientInfo],
    all_entities: &'a [(String, String)],
    process_ips: &'a [String],
    tag_registry: &'a TagRegistry,
    machine_registry: &'a MachineRegistry,
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
    run_time_budget: Duration,
}

/// Aggregated borrows needed to build workload contexts.
struct WorkloadContextEnv<'a> {
    workload_info: &'a [(String, String)],
    client_info: &'a [WorkloadClientInfo],
    all_entities: &'a [(String, String)],
    process_ips: &'a [String],
    tag_registry: &'a TagRegistry,
    machine_registry: &'a MachineRegistry,
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
    /// Virtual-time budget for the run phase. If simulated time advances past
    /// this bound while workloads are still running, the run is declared a
    /// deadlock. See [`DEFAULT_RUN_TIME_BUDGET`].
    pub(crate) run_time_budget: Duration,
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
    machine_registry: &'a MachineRegistry,
}

/// Topology metadata derived from a workload/process configuration.
struct TopologyMetadata {
    process_ips: Vec<String>,
    tag_registry: TagRegistry,
    machine_registry: MachineRegistry,
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
    machine_registry: &'a MachineRegistry,
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
    ///
    /// `process_manager` is boxed to keep this variant from dwarfing
    /// `SetupFailed` (it carries the tag + machine registries).
    Continue {
        workloads: Vec<Box<dyn Workload>>,
        contexts: Vec<SimContext>,
        process_manager: Box<ProcessManager<'pm>>,
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
    run_time_budget: Duration,
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
            run_time_budget,
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
            machine_registry,
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
                machine_registry: &machine_registry,
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
                } => (workloads, contexts, *process_manager),
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
            run_time_budget,
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
            machine_registry: &machine_registry,
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
        let machine_registry = process_config
            .map(|pc| pc.machine_registry.clone())
            .unwrap_or_default();
        let all_entities = workload_info
            .iter()
            .chain(process_config.map_or(&[][..], |pc| pc.info.as_slice()))
            .cloned()
            .collect();
        TopologyMetadata {
            process_ips,
            tag_registry,
            machine_registry,
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
            machine_registry,
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
            machine_registry,
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
            process_manager: Box::new(process_manager),
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
            run_time_budget,
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
            run_time_budget,
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

    /// Run sections 5 (abort) + 6 (settle) + 7 (check). Returns the final
    /// orchestration output.
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
            machine_registry,
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
        // Faults recorded during settle carry their own timestamps; one pump
        // suffices to flush them into the timeline.
        Self::pump_observability(sim, obs);

        // === 7. CHECK PHASE (executor spawn + cooperative stepping) ===
        let final_workloads = Self::do_check_phase(CheckPhaseInputs {
            sim,
            workloads: returned_workloads,
            workload_info,
            client_info,
            all_entities,
            process_ips,
            tag_registry,
            machine_registry,
            shutdown_signal,
            seed,
            state,
            obs,
        })
        .await
        .map_err(|()| (vec![seed], 1usize))?;
        let metrics = sim.extract_metrics();

        Ok(OrchestrateOutput {
            workloads: final_workloads,
            fault_injectors: returned_injectors,
            results,
            metrics,
        })
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
            machine_registry,
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
            machine_registry,
            shutdown_signal,
            sim,
            seed,
            state,
            obs,
        })?;
        Ok(Self::run_check_phase(sim, workloads, check_contexts, obs).await)
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
            let ip = ctx.my_ip().to_string();
            let handle = crate::executor::spawn(
                &format!("workload-setup@{ip}"),
                async move {
                    let mut w = workload;
                    let result = w.setup(&ctx).await;
                    (w, ctx, result)
                }
                .instrument(tracing::info_span!("workload", ip = %ip)),
            );
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
            let ip = ctx.my_ip().to_string();
            let handle = crate::executor::spawn(
                &format!("workload-run@{ip}"),
                async move {
                    let mut w = workload;
                    let result = w.run(&ctx).await;
                    (w, result)
                }
                .instrument(tracing::info_span!("workload", ip = %ip)),
            );
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
            run_time_budget,
        } = inputs;

        let chaos_start = sim.current_time();
        // Virtual-time origin for the run-phase budget. Both this and the
        // running `sim.current_time()` are pure functions of the event
        // schedule, so the budget trip point is bit-for-bit deterministic
        // across replays (no wall clock, no RNG).
        let mut chaos_ended = chaos_duration.is_none();
        let mut stall_guard =
            RunStallGuard::new(sim.current_time(), run_time_budget, seed, iteration_count);
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
                Self::handle_process_events(
                    sim,
                    process_manager,
                    seed,
                    state,
                    obs,
                    shutdown_signal,
                );
                Self::pump_observability(sim, obs);
            }

            let any_finished =
                Self::collect_finished_workloads(workload_handles, workload_collected).await;

            if any_finished && !shutdown_triggered {
                Self::trigger_shutdown(sim, shutdown_signal);
                shutdown_triggered = true;
            }

            Self::reap_finished_injectors(injector_handles).await;

            let current_active = workload_handles.iter().filter(|h| h.is_some()).count();

            // Evaluate both stall guards (virtual-time budget + classic
            // no-progress detector) and act on the most severe verdict.
            let stall = stall_guard.evaluate(
                sim,
                shutdown_triggered,
                current_active,
                initial_handle_count,
                initial_event_count,
            );
            match stall {
                StallOutcome::Ok => {}
                StallOutcome::Breached => {
                    Self::trigger_shutdown(sim, shutdown_signal);
                    shutdown_triggered = true;
                    stall_guard.reset_no_progress();
                }
                StallOutcome::Deadlock => return Err((vec![seed], 1)),
            }

            if current_active > 0 {
                crate::executor::until_stalled().await;
            }
        }
        // Final pump: capture events emitted after the last step.
        Self::pump_observability(sim, obs);
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
                    process_manager.process_info(),
                    crate::SimRandomProvider::new(seed),
                    sim.time_provider(),
                    chaos_shutdown.clone(),
                );
                let handle = crate::executor::spawn("fault-injector", async move {
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
                pc.machine_registry,
                all_entities.to_vec(),
            ),
            None => ProcessManager::empty(),
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
            let process_locality = pc.machine_registry.locality_for(ip_addr).cloned();
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
                my_locality: process_locality,
                machine_registry: pc.machine_registry.clone(),
                shutdown_signal: process_token.clone(),
            });
            let providers = crate::SimProviders::new(sim.downgrade(), seed, ip_addr);
            let ctx = SimContext::new(providers, topology, state.clone(), obs.clone());
            let ip_for_log = ip.clone();
            let span_ip = ip.clone();
            let handle = crate::executor::spawn(
                &format!("process@{span_ip}"),
                async move {
                    if let Err(e) = process.run(&ctx).await {
                        tracing::debug!("Process at {} exited: {}", ip_for_log, e);
                    }
                }
                .instrument(tracing::info_span!("process", ip = %span_ip)),
            );
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
        obs: &SimulationLayerHandle,
    ) -> Vec<Box<dyn Workload>> {
        let mut check_handles = Vec::with_capacity(workloads.len());
        for (workload, ctx) in workloads.into_iter().zip(contexts) {
            let ip = ctx.my_ip().to_string();
            let handle = crate::executor::spawn(
                &format!("workload-check@{ip}"),
                async move {
                    let mut w = workload;
                    let result = w.check(&ctx).await;
                    if let Err(ref e) = result {
                        tracing::error!("Workload '{}' check failed: {}", w.name(), e);
                    }
                    w
                }
                .instrument(tracing::info_span!("workload", ip = %ip)),
            );
            check_handles.push(handle);
        }

        // Cooperative loop for check.
        loop {
            if check_handles
                .iter()
                .all(crate::executor::JoinHandle::is_finished)
            {
                break;
            }
            if sim.pending_event_count() > 0 {
                sim.step();
                Self::pump_observability(sim, obs);
            }
            crate::executor::until_stalled().await;
        }
        Self::pump_observability(sim, obs);

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
                my_locality: None,
                machine_registry: env.machine_registry.clone(),
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
        workload_handles: &mut [Option<crate::executor::JoinHandle<WorkloadResult>>],
        workload_collected: &mut [Option<WorkloadResult>],
    ) -> bool {
        let mut any_finished = false;
        for i in 0..workload_handles.len() {
            let finished = workload_handles[i]
                .as_ref()
                .is_some_and(crate::executor::JoinHandle::is_finished);
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
        injector_handles: &mut [Option<crate::executor::JoinHandle<InjectorResult>>],
    ) {
        for handle_opt in injector_handles {
            let finished = handle_opt
                .as_ref()
                .is_some_and(crate::executor::JoinHandle::is_finished);
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
        mut injector_handles: Vec<Option<crate::executor::JoinHandle<InjectorResult>>>,
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
        handles: &[crate::executor::JoinHandle<T>],
    ) {
        loop {
            if handles.iter().all(crate::executor::JoinHandle::is_finished) {
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
                Self::pump_observability(sim, obs);
            }
            crate::executor::until_stalled().await;
        }
        Self::pump_observability(sim, obs);
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

    /// Current simulation time in milliseconds, saturating at `u64::MAX`.
    fn sim_now_ms(sim: &crate::sim::SimWorld) -> u64 {
        u64::try_from(sim.current_time().as_millis()).unwrap_or(u64::MAX)
    }

    /// Pump the observability pipeline after a simulation step.
    ///
    /// Pushes the sim clock into the layer (stamping subsequently captured
    /// trace events), drains engine-recorded faults into the timeline, and
    /// runs registered invariants over everything captured so far.
    fn pump_observability(sim: &crate::sim::SimWorld, obs: &SimulationLayerHandle) {
        obs.set_sim_time_ms(Self::sim_now_ms(sim));
        for record in sim.take_faults() {
            obs.record_sim_fault(record.time_ms, &record.event);
        }
        obs.run_invariants();
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
                obs.record_sim_fault(Self::sim_now_ms(sim), &event);
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
                obs.record_sim_fault(Self::sim_now_ms(sim), &event);
                process_manager.abort_process(ip);
                sim.abort_all_connections_for_ip(ip);
                sim.schedule_process_restart(ip, Duration::from_millis(recovery_delay_ms));
            }
            Some(crate::sim::Event::ProcessRestart { ip }) => {
                assert_reachable!("event: ProcessRestart");
                let event = SimFaultEvent::ProcessRestart { ip: ip.to_string() };
                obs.record_sim_fault(Self::sim_now_ms(sim), &event);
                let weak_sim = sim.downgrade();
                process_manager.restart(ip, &weak_sim, seed, state, obs, shutdown_signal);
            }
            _ => {}
        }
    }

    /// Trigger shutdown and let each simulation engine drain its own waiters.
    fn trigger_shutdown(
        sim: &mut crate::sim::SimWorld,
        shutdown_signal: &tokio_util::sync::CancellationToken,
    ) {
        tracing::debug!("Triggering shutdown signal");
        shutdown_signal.cancel();

        sim.schedule_event(crate::sim::Event::Shutdown, Duration::from_nanos(1));
    }

    /// Heal all network partitions between all IP pairs.
    fn heal_all_partitions(sim: &mut crate::sim::SimWorld, all_ips: &[String]) {
        for i in 0..all_ips.len() {
            for j in (i + 1)..all_ips.len() {
                if let (Ok(a_ip), Ok(b_ip)) = (
                    all_ips[i].parse::<std::net::IpAddr>(),
                    all_ips[j].parse::<std::net::IpAddr>(),
                ) {
                    sim.restore_partition(a_ip, b_ip);
                }
            }
        }
    }
}
