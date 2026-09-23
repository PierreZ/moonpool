//! Workload orchestration and iteration management.
//!
//! This module provides utilities for orchestrating workload execution
//! and managing simulation iterations.

use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures::FutureExt as _;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::chaos::fault_events::SimFaultEvent;
use crate::chaos::state_handle::StateHandle;
use crate::observability::SimulationLayerHandle;
use crate::providers::TaskPanicTracker;
use crate::runner::app_metrics::{MetricsHandle, SourceFactory};
use crate::runner::builder::WorkloadClientInfo;
use crate::runner::context::SimContext;
use crate::runner::fault_injector::{FaultContext, FaultInjector};
use crate::runner::groups::GroupRegistry;
use crate::runner::locality::MachineRegistry;
use crate::runner::tags::{ProcessTags, TagRegistry};
use crate::runner::topology::{TopologyFactory, TopologyInputs};
use crate::runner::workload::Workload;
use crate::sim::{ProcessKillKind, SimWorld};
use crate::{SimulationResult, assert_reachable};

use super::process_manager::{ProcessConfig, ProcessEnv, ProcessManager};
use super::report::SimulationMetrics;
use super::stall::{RunStallGuard, StallOutcome};

/// Orchestrates workload execution and event processing.
pub(crate) struct WorkloadOrchestrator;

/// Result of a completed workload task.
type WorkloadResult = (Box<dyn Workload>, SimulationResult<()>);

/// Workloads returned from one phase alongside their results.
type CompletedWorkloads = (Vec<Box<dyn Workload>>, Vec<SimulationResult<()>>);

/// Workloads, contexts and results returned by the setup phase, plus whether
/// any `setup()` failed.
type SetupOutput = (
    Vec<Box<dyn Workload>>,
    Vec<SimContext>,
    Vec<SimulationResult<()>>,
    bool,
);

/// Result returned by a spawned `setup()` task: the workload, its context,
/// and the setup result.
type SetupTaskOutput = (Box<dyn Workload>, SimContext, SimulationResult<()>);

/// Result of a completed fault injector task.
type InjectorResult = SimulationResult<()>;

/// Per-injector join handles (in option slots so they can be drained).
type InjectorHandleSlots = Vec<Option<crate::executor::JoinHandle<InjectorResult>>>;

/// Per-workload join handles (in option slots so they can be drained).
type WorkloadHandleSlots = Vec<Option<crate::executor::JoinHandle<WorkloadResult>>>;

/// A deadlocked iteration: the faulty seeds and the failed-run count.
type Deadlock = (Vec<u64>, usize);

/// The per-iteration shared borrows and scalars every phase threads through.
///
/// `Copy`, being nothing but shared borrows and plain values, so each phase
/// takes it by value.
#[derive(Clone, Copy)]
struct IterationEnv<'a> {
    metrics: &'a MetricsHandle,
    state: &'a StateHandle,
    obs: &'a SimulationLayerHandle,
    shutdown_signal: &'a CancellationToken,
    task_panics: &'a TaskPanicTracker,
    /// `(name, ip)` pairs for the workloads.
    workload_info: &'a [(String, String)],
    /// Per-workload client identity info parallel to `workload_info`.
    client_info: &'a [WorkloadClientInfo],
    topology: &'a TopologyMetadata,
    seed: u64,
    iteration_count: usize,
    run_time_budget: Duration,
}

impl IterationEnv<'_> {
    /// The deadlock verdict for this iteration: its seed, one failed run.
    fn deadlock(&self) -> Deadlock {
        (vec![self.seed], 1)
    }

    /// The environment a booted or restarted process builds its context from.
    fn process_env<'b>(&'b self, sim: &'b crate::sim::WeakSimWorld) -> ProcessEnv<'b> {
        ProcessEnv {
            sim,
            state: self.state,
            obs: self.obs,
            metrics: self.metrics,
            shutdown_signal: self.shutdown_signal,
        }
    }
}

/// A phase's stall guards, plus whether the phase already asked its actors
/// to shut down.
struct PhaseStall {
    guard: RunStallGuard,
    shutdown_triggered: bool,
}

impl PhaseStall {
    /// Arm the guards at the current simulation time.
    fn new(sim: &SimWorld, env: &IterationEnv<'_>) -> Self {
        Self {
            guard: RunStallGuard::new(
                sim.current_time(),
                env.run_time_budget,
                env.seed,
                env.iteration_count,
            ),
            shutdown_triggered: false,
        }
    }

    /// Evaluate both stall guards (virtual-time budget + classic no-progress
    /// detector) after one loop pass and act on the most severe verdict: a
    /// first breach triggers shutdown. Returns `true` on deadlock.
    fn deadlocked(
        &mut self,
        sim: &mut SimWorld,
        shutdown_signal: &CancellationToken,
        current_active: usize,
        initial_active: usize,
        initial_event_count: usize,
    ) -> bool {
        match self.guard.evaluate(
            sim,
            self.shutdown_triggered,
            current_active,
            initial_active,
            initial_event_count,
        ) {
            StallOutcome::Ok => false,
            StallOutcome::Breached => {
                WorkloadOrchestrator::trigger_shutdown(sim, shutdown_signal);
                self.shutdown_triggered = true;
                self.guard.reset_after_shutdown();
                false
            }
            StallOutcome::Deadlock => true,
        }
    }
}

/// Aggregated borrows needed to drive the run phase.
struct RunPhaseInputs<'a> {
    chaos_shutdown: &'a CancellationToken,
    chaos_duration: Option<Duration>,
    workload_handles: &'a mut WorkloadHandleSlots,
    workload_collected: &'a mut [Option<WorkloadResult>],
    workload_ips: &'a [String],
    injector_handles: &'a mut InjectorHandleSlots,
}

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
    pub(crate) sim: SimWorld,
    /// Optional chaos duration; `None` disables fault injection.
    pub(crate) chaos_duration: Option<Duration>,
    /// Builds one application-metrics source per node IP, or `None` when the
    /// simulation registered no metrics factory.
    pub(crate) metrics_factory: Option<&'a SourceFactory>,
    /// Iteration count (used for diagnostics on deadlock).
    pub(crate) iteration_count: usize,
    /// Virtual-time budget for the run phase. If simulated time advances past
    /// this bound while workloads are still running, the run is declared a
    /// deadlock. See [`DEFAULT_RUN_TIME_BUDGET`].
    pub(crate) run_time_budget: Duration,
    /// Per-iteration accounting for detached task panics.
    pub(crate) task_panics: TaskPanicTracker,
}

/// Successful output of [`WorkloadOrchestrator::orchestrate_workloads`].
pub(crate) struct OrchestrateOutput {
    /// Workloads returned to the caller for reuse.
    pub(crate) workloads: Vec<Box<dyn Workload>>,
    /// Per-workload results from setup + run + check.
    pub(crate) results: Vec<SimulationResult<()>>,
    /// Simulation metrics extracted from `sim`.
    pub(crate) metrics: SimulationMetrics,
}

/// Topology metadata derived from a workload/process configuration: the
/// per-iteration registries every workload and process context is built from.
struct TopologyMetadata {
    process_ips: Vec<String>,
    tag_registry: TagRegistry,
    machine_registry: MachineRegistry,
    group_registry: GroupRegistry,
    all_entities: Vec<(String, String)>,
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

impl WorkloadOrchestrator {
    /// Execute all workloads using the unified lifecycle:
    /// boot → setup → run (with optional chaos) → settle → check.
    ///
    /// Setup and check run inside the cooperative event loop so network
    /// RPCs don't deadlock. When `chaos_duration` is set, fault injectors
    /// run concurrently with workloads and stop when the duration elapses.
    /// After all workloads complete, a settle phase drains remaining events.
    ///
    /// Returns the workloads back to the caller for reuse across iterations;
    /// fault injectors are consumed by the run.
    pub(crate) async fn orchestrate_workloads(
        inputs: OrchestrateInputs<'_>,
    ) -> Result<OrchestrateOutput, Deadlock> {
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
            metrics_factory,
            iteration_count,
            run_time_budget,
            task_panics,
        } = inputs;

        Self::log_orchestration_start(&workloads, &fault_injectors, process_config.as_ref());

        let topology = Self::build_topology_metadata(workload_info, process_config.as_ref());

        // Shared state for cross-workload publish/get communication. Event
        // timelines and invariants live on `obs` (SimulationLayer).
        let state = StateHandle::new();
        let metrics = Self::build_metrics(metrics_factory, &topology.all_entities, &obs);
        let shutdown_signal = CancellationToken::new();
        let env = IterationEnv {
            metrics: &metrics,
            state: &state,
            obs: &obs,
            shutdown_signal: &shutdown_signal,
            task_panics: &task_panics,
            workload_info,
            client_info,
            topology: &topology,
            seed,
            iteration_count,
            run_time_budget,
        };

        let (workloads, contexts, mut process_manager) =
            match Self::boot_and_setup(&mut sim, env, process_config, workloads).await? {
                BootAndSetupOutcome::Continue {
                    workloads,
                    contexts,
                    process_manager,
                } => (workloads, contexts, *process_manager),
                BootAndSetupOutcome::SetupFailed { workloads, results } => {
                    return Ok(OrchestrateOutput {
                        workloads,
                        results,
                        metrics: Self::extract_metrics(&sim, &metrics),
                    });
                }
            };

        let (returned_workloads, results) = Self::do_chaos_and_run_phase(
            &mut sim,
            &mut process_manager,
            env,
            (workloads, contexts),
            fault_injectors,
            chaos_duration,
        )
        .await?;

        Self::finalize_orchestration(
            &mut sim,
            &mut process_manager,
            env,
            returned_workloads,
            results,
        )
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
        let group_registry = process_config
            .map(|pc| pc.group_registry.clone())
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
            group_registry,
            all_entities,
        }
    }

    /// Boot processes, build per-workload contexts, and run the setup phase.
    /// Returns either the state needed to continue into the run phase, or an
    /// early-exit signal if setup failed.
    async fn boot_and_setup<'pm>(
        sim: &mut SimWorld,
        env: IterationEnv<'_>,
        process_config: Option<ProcessConfig<'pm>>,
        workloads: Vec<Box<dyn Workload>>,
    ) -> Result<BootAndSetupOutcome<'pm>, Deadlock> {
        let mut process_manager =
            Self::boot_process_manager(process_config, sim, &env).map_err(|()| env.deadlock())?;

        let contexts = Self::build_workload_contexts(sim, &env).map_err(|()| env.deadlock())?;

        let (workloads, contexts, setup_results, setup_failed) =
            Self::do_setup_phase(workloads, contexts, sim, &mut process_manager, env)
                .await
                .map_err(|()| env.deadlock())?;
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

    /// Start the fault injectors, then drive the cooperative run loop, and
    /// collect the per-workload results.
    async fn do_chaos_and_run_phase(
        sim: &mut SimWorld,
        process_manager: &mut ProcessManager<'_>,
        env: IterationEnv<'_>,
        (workloads, contexts): (Vec<Box<dyn Workload>>, Vec<SimContext>),
        fault_injectors: Vec<Box<dyn FaultInjector>>,
        chaos_duration: Option<Duration>,
    ) -> Result<CompletedWorkloads, Deadlock> {
        let chaos_shutdown = CancellationToken::new();
        sim.start_buggified_delay_window(chaos_duration);
        let mut injector_handles = Self::start_fault_injectors(
            fault_injectors,
            chaos_duration,
            sim,
            process_manager,
            env,
            &chaos_shutdown,
        )
        .map_err(|()| env.deadlock())?;

        let total_workloads = workloads.len();
        let (handles, workload_ips) =
            Self::spawn_workload_tasks("run", workloads, contexts, |mut w, ctx| async move {
                let result = w.run(&ctx).await;
                (w, result)
            });
        let mut workload_handles: WorkloadHandleSlots = handles.into_iter().map(Some).collect();
        let mut workload_collected: Vec<Option<WorkloadResult>> =
            (0..total_workloads).map(|_| None).collect();
        Self::drive_run_phase(
            sim,
            process_manager,
            env,
            RunPhaseInputs {
                chaos_shutdown: &chaos_shutdown,
                chaos_duration,
                workload_handles: &mut workload_handles,
                workload_collected: &mut workload_collected,
                workload_ips: &workload_ips,
                injector_handles: &mut injector_handles,
            },
        )
        .await?;

        Self::abort_running_injectors(injector_handles);
        Self::collect_workload_results(workload_collected, total_workloads, &workload_ips)
            .map_err(|()| env.deadlock())
    }

    /// Abort every process, settle the world, and run the check phase.
    /// Returns the final orchestration output.
    async fn finalize_orchestration(
        sim: &mut SimWorld,
        process_manager: &mut ProcessManager<'_>,
        env: IterationEnv<'_>,
        returned_workloads: Vec<Box<dyn Workload>>,
        mut results: Vec<SimulationResult<()>>,
    ) -> Result<OrchestrateOutput, Deadlock> {
        // === 5. ABORT ALL PROCESSES ===
        process_manager.abort_all();
        // A process that panicked is a failed run, whether or not a workload
        // noticed the dead server: the panic was recorded where it happened
        // (`spawn_process`), and here it becomes part of the seed's verdict.
        for panic in process_manager.take_panics() {
            results.push(Err(crate::SimulationError::InvalidState(format!(
                "process at {} panicked: {}",
                panic.ip, panic.message
            ))));
        }

        // === 6. SETTLE ===
        if let Some(settle_err) = Self::settle_phase(sim) {
            return Ok(OrchestrateOutput {
                workloads: returned_workloads,
                results: vec![Err(settle_err)],
                metrics: Self::extract_metrics(sim, env.metrics),
            });
        }
        // Faults recorded during settle carry their own timestamps; one pump
        // suffices to flush them into the timeline.
        Self::pump_observability(sim, env.obs);

        // === 7. CHECK PHASE (executor spawn + cooperative stepping) ===
        let (final_workloads, check_results) = Self::do_check_phase(sim, returned_workloads, env)
            .await
            .map_err(|()| env.deadlock())?;
        // A `check()` verdict is part of the iteration's result: a workload
        // whose `run()` succeeded but whose final validation returned `Err`
        // (or panicked) fails the seed exactly as a failing `run()` does.
        results.extend(check_results);
        // Scraped last, after `check()` has run: a workload that drives its
        // final requests from `check()` still has them counted.
        let sim_metrics = Self::extract_metrics(sim, env.metrics);

        Ok(OrchestrateOutput {
            workloads: final_workloads,
            results,
            metrics: sim_metrics,
        })
    }

    /// Run the entire check phase: build per-workload contexts, spawn
    /// `check()` futures, drive the cooperative loop, and collect the
    /// resulting workloads beside each one's `check()` verdict.
    ///
    /// A `check()` that does not return its owned workload stops the campaign,
    /// because later seeds cannot safely recreate an instance workload.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if a workload IP fails to parse or a check task is
    /// lost.
    async fn do_check_phase(
        sim: &mut SimWorld,
        workloads: Vec<Box<dyn Workload>>,
        env: IterationEnv<'_>,
    ) -> Result<CompletedWorkloads, ()> {
        let check_contexts = Self::build_workload_contexts(sim, &env)?;
        let (check_handles, check_ips) = Self::spawn_workload_tasks(
            "check",
            workloads,
            check_contexts,
            |mut w, ctx| async move {
                let result = w.check(&ctx).await;
                if let Err(ref e) = result {
                    tracing::error!("Workload '{}' check failed: {}", w.name(), e);
                }
                (w, result)
            },
        );

        if !Self::cooperative_loop_until_done(sim, None, env, &check_handles).await {
            for handle in &check_handles {
                handle.abort();
            }
        }
        Self::pump_observability(sim, env.obs);

        // Collect check results.
        let mut final_workloads = Vec::with_capacity(check_handles.len());
        let mut check_results = Vec::with_capacity(check_handles.len());
        for (index, handle) in check_handles.into_iter().enumerate() {
            match handle.await {
                Ok((workload, result)) => {
                    final_workloads.push(workload);
                    check_results.push(result);
                }
                Err(error) => {
                    Self::log_lost_workload_task("check", &check_ips[index], &error);
                    return Err(());
                }
            }
        }
        Ok((final_workloads, check_results))
    }

    /// Run the entire setup phase: spawn `setup()` futures, drive the
    /// cooperative loop, and collect results.
    async fn do_setup_phase(
        workloads: Vec<Box<dyn Workload>>,
        contexts: Vec<SimContext>,
        sim: &mut SimWorld,
        process_manager: &mut ProcessManager<'_>,
        env: IterationEnv<'_>,
    ) -> Result<SetupOutput, ()> {
        let (setup_handles, setup_ips) =
            Self::spawn_workload_tasks("setup", workloads, contexts, |mut w, ctx| async move {
                let result = w.setup(&ctx).await;
                (w, ctx, result)
            });
        let done =
            Self::cooperative_loop_until_done(sim, Some(process_manager), env, &setup_handles)
                .await;
        Self::pump_observability(sim, env.obs);
        if !done {
            for handle in &setup_handles {
                handle.abort();
            }
        }
        Self::collect_setup_results(setup_handles, &setup_ips).await
    }

    /// Spawn one task per workload running `task(workload, context)` inside
    /// the workload's span, named `workload-{phase}@{ip}`. Returns the join
    /// handles and the workload IPs, both in workload order.
    fn spawn_workload_tasks<T, Fut>(
        phase: &str,
        workloads: Vec<Box<dyn Workload>>,
        contexts: Vec<SimContext>,
        task: impl Fn(Box<dyn Workload>, SimContext) -> Fut,
    ) -> (Vec<crate::executor::JoinHandle<T>>, Vec<String>)
    where
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let mut handles = Vec::with_capacity(workloads.len());
        let mut ips = Vec::with_capacity(workloads.len());
        for (workload, ctx) in workloads.into_iter().zip(contexts) {
            let ip = ctx.my_ip().to_string();
            let handle = crate::executor::spawn(
                &format!("workload-{phase}@{ip}"),
                task(workload, ctx).instrument(tracing::info_span!("workload", ip = %ip)),
            );
            handles.push(handle);
            ips.push(ip);
        }
        (handles, ips)
    }

    /// Drive the unified cooperative run-phase loop until every workload
    /// task has completed (or a deadlock forces the loop to bail out).
    async fn drive_run_phase(
        sim: &mut SimWorld,
        process_manager: &mut ProcessManager<'_>,
        env: IterationEnv<'_>,
        inputs: RunPhaseInputs<'_>,
    ) -> Result<(), Deadlock> {
        let RunPhaseInputs {
            chaos_shutdown,
            chaos_duration,
            workload_handles,
            workload_collected,
            workload_ips,
            injector_handles,
        } = inputs;

        let chaos_start = sim.current_time();
        // Virtual-time origin for the run-phase budget. Both this and the
        // running `sim.current_time()` are pure functions of the event
        // schedule, so the budget trip point is bit-for-bit deterministic
        // across replays (no wall clock, no RNG).
        let mut chaos_ended = chaos_duration.is_none();
        let mut stall = PhaseStall::new(sim, &env);
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
                // Stop the two sources of new faults together: the explicit
                // injector loops (token) and the configuration-driven network,
                // storage, and block-device families (recovery mode, which
                // also heals the partitions the simulator is holding). Damage
                // already done stays done — the quiet tail that follows is
                // where the system under test has to recover from it, while
                // its processes are still alive.
                chaos_shutdown.cancel();
                sim.enter_recovery_mode();
                chaos_ended = true;
                assert_reachable!("phase: chaos ended");
            }

            Self::step_and_pump(sim, Some(&mut *process_manager), &env);

            let any_finished = Self::collect_finished_workloads(
                workload_handles,
                workload_collected,
                workload_ips,
            )
            .await
            .map_err(|()| env.deadlock())?;

            if any_finished && !stall.shutdown_triggered {
                Self::trigger_shutdown(sim, env.shutdown_signal);
                stall.shutdown_triggered = true;
            }

            Self::reap_finished_injectors(injector_handles).await;

            let current_active = workload_handles.iter().filter(|h| h.is_some()).count();
            if stall.deadlocked(
                sim,
                env.shutdown_signal,
                current_active,
                initial_handle_count,
                initial_event_count,
            ) {
                return Err(env.deadlock());
            }

            if current_active > 0 {
                crate::executor::until_stalled().await;
            }
        }
        // Final pump: capture events emitted after the last step.
        Self::pump_observability(sim, env.obs);
        Ok(())
    }

    /// Spawn the fault injectors for the chaos phase. When `chaos_duration`
    /// is `None`, the injectors are dropped unrun.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if the simulation has already been shut down.
    fn start_fault_injectors(
        fault_injectors: Vec<Box<dyn FaultInjector>>,
        chaos_duration: Option<Duration>,
        sim: &SimWorld,
        process_manager: &ProcessManager<'_>,
        env: IterationEnv<'_>,
        chaos_shutdown: &CancellationToken,
    ) -> Result<InjectorHandleSlots, ()> {
        let mut injector_handles: InjectorHandleSlots = Vec::new();
        if chaos_duration.is_none() {
            return Ok(injector_handles);
        }
        for mut injector in fault_injectors {
            let fault_sim = sim.downgrade().upgrade().map_err(|_| ())?;
            let fault_ctx = FaultContext::new(
                fault_sim,
                process_manager.process_info(),
                crate::SimRandomProvider::new(),
                sim.time_provider(),
                env.state.clone(),
                chaos_shutdown.clone(),
            );
            let reporter = env.task_panics.reporter("fault-injector");
            let handle = crate::executor::spawn("fault-injector", async move {
                match AssertUnwindSafe(injector.inject(&fault_ctx))
                    .catch_unwind()
                    .await
                {
                    Ok(result) => result,
                    Err(payload) => {
                        reporter.record("root", payload.as_ref());
                        Ok(())
                    }
                }
            });
            injector_handles.push(Some(handle));
        }
        Ok(injector_handles)
    }

    /// Boot the configured processes under a [`ProcessManager`] for lifecycle
    /// management. Returns an empty manager when `process_config` is `None`.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if a process IP fails to parse during boot.
    fn boot_process_manager<'pm>(
        process_config: Option<ProcessConfig<'pm>>,
        sim: &SimWorld,
        env: &IterationEnv<'_>,
    ) -> Result<ProcessManager<'pm>, ()> {
        let Some(pc) = process_config else {
            return Ok(ProcessManager::empty());
        };
        let mut process_manager = ProcessManager::new(
            pc,
            env.topology.all_entities.clone(),
            env.task_panics.clone(),
        );
        process_manager.boot_all(&env.process_env(&sim.downgrade()))?;
        Ok(process_manager)
    }

    /// Build per-workload [`SimContext`]s for the setup/run and check phases.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` if a workload IP fails to parse.
    fn build_workload_contexts(
        sim: &SimWorld,
        env: &IterationEnv<'_>,
    ) -> Result<Vec<SimContext>, ()> {
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
                all_entities: &env.topology.all_entities,
                process_ips: &env.topology.process_ips,
                my_tags: ProcessTags::default(),
                tag_registry: env.topology.tag_registry.clone(),
                my_locality: None,
                machine_registry: env.topology.machine_registry.clone(),
                group_registry: env.topology.group_registry.clone(),
                shutdown_signal: env.shutdown_signal.clone(),
            });
            let providers = crate::SimProviders::new(sim.downgrade(), ip_addr)
                .with_task_panic_reporter(env.task_panics.reporter(format!("workload@{ip}")));
            let ctx = SimContext::new(
                providers,
                topology,
                env.state.clone(),
                env.obs.clone(),
                env.metrics.clone(),
            );
            contexts.push(ctx);
        }
        Ok(contexts)
    }

    /// Returns `true` once enough simulation time has elapsed to end the
    /// chaos phase, given the chaos start time and configured duration.
    fn should_end_chaos(
        sim: &SimWorld,
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
        workload_ips: &[String],
    ) -> Result<bool, ()> {
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
                    Err(error) => {
                        Self::log_lost_workload_task("run", &workload_ips[i], &error);
                        return Err(());
                    }
                }
                any_finished = true;
            }
        }
        Ok(any_finished)
    }

    /// Reap any fault-injector handles that have finished, dropping the
    /// injectors with them. The remaining live handles stay in place.
    async fn reap_finished_injectors(injector_handles: &mut InjectorHandleSlots) {
        for handle_opt in injector_handles {
            let finished = handle_opt
                .as_ref()
                .is_some_and(crate::executor::JoinHandle::is_finished);
            if finished {
                let handle = handle_opt.take().expect("injector handle is finished");
                match handle.await {
                    Ok(_result) => {
                        tracing::debug!("Fault injector completed");
                    }
                    Err(_) => {
                        tracing::error!("Fault injector task panicked");
                    }
                }
            }
        }
    }

    /// Abort every fault-injector task still running once the run phase is
    /// over; finished ones were reaped on the loop.
    fn abort_running_injectors(injector_handles: InjectorHandleSlots) {
        for handle in injector_handles.into_iter().flatten() {
            handle.abort();
        }
    }

    /// Build the final workload return list when every task returned its
    /// workload. A missing slot ends the campaign before compacting can shift
    /// a later workload into an earlier entry's identity.
    fn collect_workload_results(
        workload_collected: Vec<Option<WorkloadResult>>,
        total_workloads: usize,
        workload_ips: &[String],
    ) -> Result<CompletedWorkloads, ()> {
        let mut returned_workloads = Vec::with_capacity(total_workloads);
        let mut results = Vec::with_capacity(total_workloads);

        for (index, item) in workload_collected.into_iter().enumerate() {
            if let Some((workload, result)) = item {
                returned_workloads.push(workload);
                results.push(result);
            } else {
                Self::log_lost_workload_task(
                    "run",
                    &workload_ips[index],
                    &moonpool_core::JoinError::Cancelled,
                );
                return Err(());
            }
        }
        Ok((returned_workloads, results))
    }

    /// Drive the simulation cooperatively until every handle in `handles`
    /// reports finished. Returns `false` when the phase deadlocked first; the
    /// caller does the final observability pump and aborts the stragglers.
    ///
    /// With a `process_manager`, the process lifecycle events of every step
    /// are handled (setup); without one (check, after every process was
    /// aborted) they are not.
    async fn cooperative_loop_until_done<T: 'static>(
        sim: &mut SimWorld,
        mut process_manager: Option<&mut ProcessManager<'_>>,
        env: IterationEnv<'_>,
        handles: &[crate::executor::JoinHandle<T>],
    ) -> bool {
        let active = || {
            handles
                .iter()
                .filter(|handle| !handle.is_finished())
                .count()
        };
        let mut stall = PhaseStall::new(sim, &env);
        loop {
            let active_handles = active();
            if active_handles == 0 {
                return true;
            }
            let initial_event_count = sim.pending_event_count();
            Self::step_and_pump(sim, process_manager.as_deref_mut(), &env);
            if stall.deadlocked(
                sim,
                env.shutdown_signal,
                active(),
                active_handles,
                initial_event_count,
            ) {
                return false;
            }
            crate::executor::until_stalled().await;
        }
    }

    /// Process one simulation event when any is pending: step, handle the
    /// process lifecycle event it produced (when processes are live), and
    /// pump observability.
    fn step_and_pump(
        sim: &mut SimWorld,
        process_manager: Option<&mut ProcessManager<'_>>,
        env: &IterationEnv<'_>,
    ) {
        if sim.pending_event_count() > 0 {
            sim.step();
            if let Some(process_manager) = process_manager {
                Self::handle_process_events(sim, process_manager, env);
            }
            Self::pump_observability(sim, env.obs);
        }
    }

    /// Collect results from spawned `setup()` tasks.
    async fn collect_setup_results(
        setup_handles: Vec<crate::executor::JoinHandle<SetupTaskOutput>>,
        setup_ips: &[String],
    ) -> Result<SetupOutput, ()> {
        let mut workloads = Vec::with_capacity(setup_handles.len());
        let mut contexts = Vec::with_capacity(setup_handles.len());
        let mut setup_failed = false;
        let mut setup_results: Vec<SimulationResult<()>> = Vec::new();
        for (index, handle) in setup_handles.into_iter().enumerate() {
            match handle.await {
                Ok((workload, context, result)) => {
                    if let Err(ref error) = result {
                        tracing::error!("Workload '{}' setup failed: {error}", workload.name());
                        setup_failed = true;
                    }
                    setup_results.push(result);
                    workloads.push(workload);
                    contexts.push(context);
                }
                Err(error) => {
                    Self::log_lost_workload_task("setup", &setup_ips[index], &error);
                    return Err(());
                }
            }
        }
        Ok((workloads, contexts, setup_results, setup_failed))
    }

    /// Record why a workload task cannot return the instance it owns.
    fn log_lost_workload_task(phase: &'static str, ip: &str, error: &moonpool_core::JoinError) {
        let span = tracing::info_span!("workload", ip = %ip);
        span.in_scope(|| {
            tracing::error!(
                phase,
                cause = %error,
                "workload_task_lost"
            );
        });
    }

    /// Current simulation time in milliseconds, saturating at `u64::MAX`.
    fn sim_now_ms(sim: &SimWorld) -> u64 {
        u64::try_from(sim.current_time().as_millis()).unwrap_or(u64::MAX)
    }

    /// Pump the observability pipeline after a simulation step.
    ///
    /// Pushes the sim clock into the layer (stamping subsequently captured
    /// trace events), drains engine-recorded faults into the timeline, and
    /// runs registered invariants over everything captured so far.
    fn pump_observability(sim: &SimWorld, obs: &SimulationLayerHandle) {
        obs.set_sim_time_ms(Self::sim_now_ms(sim));
        for record in sim.take_faults() {
            obs.record_sim_fault(record.time_ms, &record.event);
        }
        obs.run_invariants();
    }

    /// Drain remaining simulation events synchronously after all workloads
    /// have completed.
    ///
    /// This is **not** a recovery window. `abort_all()` has already killed
    /// every process by the time settle runs, so no protocol work can happen
    /// here — it only surfaces cleanup bugs in what the run left behind. The
    /// window in which the system under test can actually recover from chaos
    /// is the quiet tail between the `chaos_duration` cutoff and workload
    /// completion, while the processes are still alive.
    ///
    /// Returns `Some(SettleTimeout)` if the queue does not converge within
    /// the timeout, otherwise `None` on a clean drain.
    fn settle_phase(sim: &mut SimWorld) -> Option<crate::SimulationError> {
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

    /// Log the shape of the iteration about to run.
    fn log_orchestration_start(
        workloads: &[Box<dyn Workload>],
        fault_injectors: &[Box<dyn FaultInjector>],
        process_config: Option<&ProcessConfig<'_>>,
    ) {
        tracing::debug!(
            "Orchestrating {} workload(s), {} fault injector(s), {} process(es)",
            workloads.len(),
            fault_injectors.len(),
            process_config.map_or(0, |pc| pc.ips.len()),
        );
    }

    /// Build this iteration's application-metrics handle: one source per node,
    /// rebuilt per iteration so counters start at zero for every seed, and
    /// armed against the simulated clock. Empty when no factory was
    /// registered, which is the no-metrics-configured case.
    fn build_metrics(
        factory: Option<&SourceFactory>,
        all_entities: &[(String, String)],
        obs: &SimulationLayerHandle,
    ) -> MetricsHandle {
        factory.map_or_else(MetricsHandle::new, |factory| {
            MetricsHandle::from_factory(
                factory,
                all_entities.iter().map(|(_, ip)| ip.as_str()),
                obs,
            )
        })
    }

    /// Snapshot engine metrics and scrape every node's application metrics
    /// into one [`SimulationMetrics`].
    ///
    /// The scrape happens once per iteration rather than per step: reading a
    /// registry is an observation, and doing it on every step would cost more
    /// than it reveals while producing a timeline nothing consumes.
    fn extract_metrics(sim: &SimWorld, metrics: &MetricsHandle) -> SimulationMetrics {
        let mut out = sim.extract_metrics();
        out.app_metrics = metrics.collect_all();
        out.app_series = metrics.collect_series();
        out.dropped_metric_points = metrics.dropped_points();
        out
    }

    /// Handle process lifecycle events from the last simulation step.
    fn handle_process_events(
        sim: &mut SimWorld,
        process_manager: &mut ProcessManager<'_>,
        env: &IterationEnv<'_>,
    ) {
        let obs = env.obs;
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
                        recovery_delay_ms: Some(recovery_delay_ms),
                        cause: ProcessKillKind::GracePeriodExpired,
                    },
                    Duration::from_millis(grace_period_ms),
                );
            }
            Some(crate::sim::Event::ProcessForceKill {
                ip,
                recovery_delay_ms,
                cause,
            }) => {
                assert_reachable!("event: ProcessForceKill");
                let event = SimFaultEvent::ProcessForceKill {
                    ip: ip.to_string(),
                    cause,
                };
                obs.record_sim_fault(Self::sim_now_ms(sim), &event);
                // Abort the task *first*: aborting connections and crashing
                // storage wake the process, and a dead process must not run
                // application work during its recovery delay.
                process_manager.abort_process(ip);
                sim.abort_all_connections_for_ip(ip);
                match cause {
                    ProcessKillKind::GracePeriodExpired | ProcessKillKind::RestartInPlace => {}
                    ProcessKillKind::Crash => sim.simulate_crash_for_process(ip, true),
                    ProcessKillKind::CrashAndWipe => {
                        sim.simulate_crash_for_process(ip, true);
                        sim.wipe_storage_for_process(ip);
                    }
                }
                // A held-down crash (None) schedules no restart: the process
                // stays dead until a fault injector explicitly restarts it.
                if let Some(recovery_delay_ms) = recovery_delay_ms {
                    sim.schedule_process_restart(ip, Duration::from_millis(recovery_delay_ms));
                }
            }
            Some(crate::sim::Event::ProcessRestart { ip }) => {
                assert_reachable!("event: ProcessRestart");
                if process_manager.is_running(ip) {
                    // Restarting a running process is a kill, then a boot.
                    // Killing cancels the root task, but the executor drops
                    // a cancelled future only when it next runs it; booting
                    // the replacement in the same step could poll it first,
                    // beside the old boot's live state (a listener, a
                    // registry, open connections). Kill now, like a force
                    // kill, and boot one tick later, after the executor
                    // has drained the old boot: two boots of one process
                    // never overlap.
                    // Recorded as a force kill so the aborted connections
                    // have a cause in the fault timeline.
                    obs.record_sim_fault(
                        Self::sim_now_ms(sim),
                        &SimFaultEvent::ProcessForceKill {
                            ip: ip.to_string(),
                            cause: ProcessKillKind::RestartInPlace,
                        },
                    );
                    process_manager.abort_process(ip);
                    sim.abort_all_connections_for_ip(ip);
                    sim.schedule_process_restart(ip, Duration::from_nanos(1));
                    assert_reachable!(
                        "process_manager: running process stopped before its restart"
                    );
                    return;
                }
                let event = SimFaultEvent::ProcessRestart { ip: ip.to_string() };
                obs.record_sim_fault(Self::sim_now_ms(sim), &event);
                let weak_sim = sim.downgrade();
                // The restarted process keeps its node's metrics source: the
                // IP is unchanged, so counters survive the reboot exactly as a
                // real node's do across a process restart on the same host.
                process_manager.restart(ip, &env.process_env(&weak_sim));
            }
            _ => {}
        }
    }

    /// Trigger shutdown and let each simulation engine drain its own waiters.
    fn trigger_shutdown(sim: &mut SimWorld, shutdown_signal: &CancellationToken) {
        tracing::debug!("Triggering shutdown signal");
        shutdown_signal.cancel();

        sim.schedule_event(crate::sim::Event::Shutdown, Duration::from_nanos(1));
    }
}
